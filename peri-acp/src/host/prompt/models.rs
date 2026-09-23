//! Model factories share the captured provider configuration and the session pool.
use crate::{
    provider::{LlmProvider, PeriConfig},
    session::{
        agent_pool::{AgentPool, CachedLlmInstances},
        executor,
        retry_events::RetryEventForwarder,
    },
};
use std::sync::Arc;

type ModelFactory = Arc<dyn Fn() -> Arc<dyn peri_model::Model> + Send + Sync>;
type CacheReader = Arc<dyn Fn() -> Option<CachedLlmInstances> + Send + Sync>;
type CacheWriter = Arc<dyn Fn(CachedLlmInstances) + Send + Sync>;

/// 每次 LLM 调用都重读 provider 的 `Model` 代理。
///
/// ## 为什么需要它
///
/// peri 原本在 prompt 装配时把 provider 快照进 `StageContext`，整个 ReAct
/// 循环共用同一个 LLM 实例——运行中切模型（TUI 的 `/model` 面板）只有**下一个
/// 用户轮**才生效。pi 的做法不同：`prepareNextTurnWithContext` 在每轮 LLM 调用
/// 前返回 `model: agent.state.model`，所以**下一个 ReAct 迭代**（工具调用之后）
/// 就换模型。
///
/// 本代理把「解析当前 provider → 取/建缓存实例」从装配期推迟到**每次调用**，
/// 于是 peri 与 pi 行为对齐。fingerprint 命中时仍复用缓存实例，peri 的跨轮
/// 热缓存（reqwest 连接池 + TLS session cache）设计不受影响。
pub(super) struct SwitchingModel {
    provider: SharedProvider,
    pool: Arc<parking_lot::Mutex<AgentPool>>,
    retry_events: RetryEventForwarder,
}

impl SwitchingModel {
    /// 按**当前** provider 解析出应使用的实例（fingerprint 命中则复用缓存）。
    ///
    /// `provider.read().clone()` 的读锁在语句结束时释放，不会跨 await 持有。
    fn resolve(&self) -> Arc<dyn peri_model::Model> {
        let live = self.provider.read().clone();
        let fp = crate::session::agent_pool::fingerprint(&live);
        crate::session::agent_pool::AgentPool::get_or_create_subagent_llm(
            &self.pool,
            &fp,
            || {
                live.clone()
                    .with_retry_observer(Some(self.retry_events.as_retry_observer()))
                    .into_model()
            },
        )
    }
}

#[async_trait::async_trait]
impl peri_model::Model for SwitchingModel {
    fn capabilities(&self) -> peri_model::ModelCapabilities {
        self.resolve().capabilities()
    }

    fn prepare_request(
        &self,
        request: &peri_model::ModelRequest,
    ) -> peri_model::ModelResult<peri_model::PreparedModelRequest> {
        self.resolve().prepare_request(request)
    }

    async fn stream(
        &self,
        request: peri_model::ModelRequest,
        cancellation: tokio_util::sync::CancellationToken,
    ) -> peri_model::ModelResult<peri_model::ModelStream> {
        // 在这里（而不是装配时）解析：切模型后的下一个 ReAct 迭代就会拿到新实例
        self.resolve().stream(request, cancellation).await
    }
}

pub(super) struct ModelFactories {
    pub get_cached_llm: Option<CacheReader>,
    pub fresh_auxiliary_model: Option<ModelFactory>,
    pub store_llm: Option<CacheWriter>,
    pub primary_llm_factory: Option<ModelFactory>,
    pub auto_classifier_factory: Option<executor::AutoClassifierFactory>,
    pub subagent_llm_factory: Option<executor::SubagentLlmFactory>,
}

/// 共享的 provider 句柄。
///
/// **不再传快照**：模型要能在**运行中**切换（TUI 的 /model 面板）并让下一个
/// ReAct 迭代就用新模型。快照会把本轮锁死在装配时的模型上。
/// 这与 pi 的 `prepareNextTurnWithContext` 每轮返回 `agent.state.model` 等价。
type SharedProvider = Arc<parking_lot::RwLock<LlmProvider>>;

pub(super) fn build_model_factories(
    provider: &SharedProvider,
    peri_config_snapshot: &Arc<PeriConfig>,
    pool: &Arc<parking_lot::Mutex<AgentPool>>,
    retry_events: &RetryEventForwarder,
    session_id: &str,
) -> ModelFactories {
    // 主 LLM 缓存读取（AgentPool has_valid_cache + get_cached_llm 语义）
    let get_cached_llm: Option<Arc<dyn Fn() -> Option<CachedLlmInstances> + Send + Sync>> = {
        let pool = Arc::clone(pool);
        let provider = Arc::clone(provider);
        Some(Arc::new(move || {
            // 读 live provider：运行中切模型后 fingerprint 变，缓存自然失效
            let live = provider.read().clone();
            let guard = pool.lock();
            if guard.has_valid_cache(&live) {
                guard.get_cached_llm().cloned()
            } else {
                None
            }
        }))
    };
    // fresh auxiliary model（缓存缺失时；retry observer 烘焙）
    let fresh_auxiliary_model: Option<Arc<dyn Fn() -> Arc<dyn peri_model::Model> + Send + Sync>> = {
        let pool = Arc::clone(pool);
        let provider = Arc::clone(provider);
        Some(Arc::new(move || {
            let live = provider
                .read()
                .clone()
                .with_retry_observer(Some(pool.lock().retry_events.as_retry_observer()));
            live.into_model().into()
        }))
    };
    // LLM 缓存回写（AgentPool store_llm 语义）
    let store_llm: Option<Arc<dyn Fn(CachedLlmInstances) + Send + Sync>> = {
        let pool = Arc::clone(pool);
        Some(Arc::new(move |cache: CachedLlmInstances| {
            pool.lock().store_llm(cache);
        }))
    };
    // stage 装配 LLM 工厂（主 LLM / auto-classifier / 子 agent；与迁移前
    // stage_builder 桥内构造同源——AgentPool 缓存 + RetryObserver 烘焙）
    let primary_llm_factory: Option<Arc<dyn Fn() -> Arc<dyn peri_model::Model> + Send + Sync>> = {
        let pool = Arc::clone(pool);
        let provider = Arc::clone(provider);
        let retry_events = retry_events.clone();
        Some(Arc::new(move || {
            // 返回代理而不是已解析的实例：让每次 stream 重新按当前 provider
            // 解析（运行中切模型 → 下一个 ReAct 迭代生效）。
            Arc::new(SwitchingModel {
                provider: Arc::clone(&provider),
                pool: Arc::clone(&pool),
                retry_events: retry_events.clone(),
            }) as Arc<dyn peri_model::Model>
        }))
    };
    let auto_classifier_factory: Option<executor::AutoClassifierFactory> = {
        let provider = Arc::clone(provider);
        let retry_events = retry_events.clone();
        Some(Arc::new(move || {
            let live = provider
                .read()
                .clone()
                .with_retry_observer(Some(retry_events.as_retry_observer()));
            Arc::new(tokio::sync::Mutex::new(live.into_model()))
        }))
    };
    let subagent_llm_factory: Option<executor::SubagentLlmFactory> = {
        let provider = Arc::clone(provider);
        let peri_config = Arc::clone(peri_config_snapshot);
        let pool = Arc::clone(pool);
        let retry_events = retry_events.clone();
        let sid = session_id.to_owned();
        Some(Arc::new(move |model_alias: Option<&str>| {
            // 解析 provider 并构建 fingerprint。无 alias 时用 live 主 provider
            // （之前用装配时快照，运行中切模型后子 agent 会沿用旧模型）。
            let live_main = provider.read().clone();
            let (p, fp) = if let Some(alias) = model_alias {
                match LlmProvider::from_config_for_alias(&peri_config, alias) {
                    Some(p) => {
                        let fp = crate::session::agent_pool::fingerprint(&p);
                        (Some(p), fp)
                    }
                    None => {
                        let fp = crate::session::agent_pool::fingerprint(&live_main);
                        (None, fp)
                    }
                }
            } else {
                let fp = crate::session::agent_pool::fingerprint(&live_main);
                (None, fp)
            };
            // 尝试 SubAgent 缓存
            let model: Arc<dyn peri_model::Model> =
                crate::session::agent_pool::AgentPool::get_or_create_subagent_llm(
                    &pool,
                    &fp,
                    || match &p {
                        Some(p) => p
                            .clone()
                            .with_retry_observer(Some(retry_events.as_retry_observer()))
                            .into_model(),
                        None => live_main
                            .clone()
                            .with_retry_observer(Some(retry_events.as_retry_observer()))
                            .into_model(),
                    },
                );
            let mut llm = peri_agent::agent::model_bridge::AgentModelBridge::from_arc(model);
            llm = llm.with_session_id(sid.clone());
            Box::new(llm)
        }))
    };

    ModelFactories {
        get_cached_llm,
        fresh_auxiliary_model,
        store_llm,
        primary_llm_factory,
        auto_classifier_factory,
        subagent_llm_factory,
    }
}
