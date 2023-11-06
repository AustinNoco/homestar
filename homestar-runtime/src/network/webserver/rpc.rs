use super::{listener, prom::PrometheusData};
#[cfg(feature = "websocket-notify")]
use super::{
    notifier::{self, Header, Notifier, SubscriptionTyp},
    Message,
};
use crate::runner::WsSender;
#[cfg(feature = "websocket-notify")]
use anyhow::anyhow;
use anyhow::Result;
#[cfg(feature = "websocket-notify")]
use dashmap::DashMap;
#[cfg(feature = "websocket-notify")]
use faststr::FastStr;
#[cfg(feature = "websocket-notify")]
use futures::StreamExt;
use jsonrpsee::{
    server::RpcModule,
    types::{error::ErrorCode, ErrorObjectOwned},
};
#[cfg(feature = "websocket-notify")]
use jsonrpsee::{types::SubscriptionId, SubscriptionMessage, SubscriptionSink, TrySendError};
#[cfg(feature = "websocket-notify")]
use libipld::Cid;
use metrics_exporter_prometheus::PrometheusHandle;
#[cfg(feature = "websocket-notify")]
use std::sync::Arc;
use std::time::Duration;
#[cfg(feature = "websocket-notify")]
use tokio::{
    runtime::Handle,
    select,
    sync::oneshot,
    time::{self, Instant},
};
#[cfg(feature = "websocket-notify")]
use tokio_stream::wrappers::BroadcastStream;
#[cfg(feature = "websocket-notify")]
use tracing::{error, info, warn};

/// Health endpoint.
pub(crate) const HEALTH_ENDPOINT: &str = "health";
/// Metrics endpoint for prometheus / openmetrics polling.
pub(crate) const METRICS_ENDPOINT: &str = "metrics";
/// Run a workflow and subscribe to that workflow's events.
#[cfg(feature = "websocket-notify")]
pub(crate) const SUBSCRIBE_RUN_WORKFLOW_ENDPOINT: &str = "subscribe_run_workflow";
/// Unsubscribe from a workflow's events.
#[cfg(feature = "websocket-notify")]
pub(crate) const UNSUBSCRIBE_RUN_WORKFLOW_ENDPOINT: &str = "unsubscribe_run_workflow";
/// Subscribe to network events.
#[cfg(feature = "websocket-notify")]
pub(crate) const SUBSCRIBE_NETWORK_EVENTS_ENDPOINT: &str = "subscribe_network_events";
/// Unsubscribe from network events.
#[cfg(feature = "websocket-notify")]
pub(crate) const UNSUBSCRIBE_NETWORK_EVENTS_ENDPOINT: &str = "unsubscribe_network_events";

/// TODO
#[cfg(feature = "websocket-notify")]
pub(crate) struct Context {
    metrics_hdl: PrometheusHandle,
    evt_notifier: Notifier<notifier::Message>,
    workflow_msg_notifier: Notifier<notifier::Message>,
    runner_sender: WsSender,
    receiver_timeout: Duration,
    workflow_listeners: Arc<DashMap<SubscriptionId<'static>, (Cid, FastStr)>>,
}

/// TODO
#[allow(dead_code)]
#[cfg(not(feature = "websocket-notify"))]
pub(crate) struct Context {
    metrics_hdl: PrometheusHandle,
    runner_sender: WsSender,
    receiver_timeout: Duration,
}

impl Context {
    /// TODO
    #[cfg(feature = "websocket-notify")]
    #[cfg_attr(docsrs, doc(cfg(feature = "websocket-notify")))]
    pub(crate) fn new(
        metrics_hdl: PrometheusHandle,
        evt_notifier: Notifier<notifier::Message>,
        workflow_msg_notifier: Notifier<notifier::Message>,
        runner_sender: WsSender,
        receiver_timeout: Duration,
    ) -> Self {
        Self {
            metrics_hdl,
            evt_notifier,
            workflow_msg_notifier,
            runner_sender,
            receiver_timeout,
            workflow_listeners: DashMap::new().into(),
        }
    }

    /// TODO
    #[cfg(not(feature = "websocket-notify"))]
    pub(crate) fn new(
        metrics_hdl: PrometheusHandle,
        runner_sender: WsSender,
        receiver_timeout: Duration,
    ) -> Self {
        Self {
            metrics_hdl,
            runner_sender,
            receiver_timeout,
        }
    }
}

/// [RpcModule] wrapper.
pub(crate) struct JsonRpc(RpcModule<Context>);

impl JsonRpc {
    /// Create a new [JsonRpc] instance, registering methods on initialization.
    pub(crate) async fn new(ctx: Context) -> Result<Self> {
        let module = Self::register(ctx).await?;
        Ok(Self(module))
    }

    /// Get a reference to the inner [RpcModule].
    #[allow(dead_code)]
    pub(crate) fn inner(&self) -> &RpcModule<Context> {
        &self.0
    }

    /// Get and take ownership of the inner [RpcModule].
    pub(crate) fn into_inner(self) -> RpcModule<Context> {
        self.0
    }

    async fn register(ctx: Context) -> Result<RpcModule<Context>> {
        let mut module = RpcModule::new(ctx);

        module.register_async_method(HEALTH_ENDPOINT, |_, _| async move {
            serde_json::json!({ "healthy": true })
        })?;

        module.register_async_method(METRICS_ENDPOINT, |params, ctx| async move {
            let render = ctx.metrics_hdl.render();

            // TODO: Handle prefix specific metrics in parser.
            match params.one::<listener::MetricsPrefix>() {
                Ok(listener::MetricsPrefix { prefix: _prefix }) => {
                    PrometheusData::from_string(&render)
                        .map_err(|_err| ErrorObjectOwned::from(ErrorCode::InternalError))
                }
                Err(_) => PrometheusData::from_string(&render)
                    .map_err(|_err| ErrorObjectOwned::from(ErrorCode::InternalError)),
            }
        })?;

        #[cfg(feature = "websocket-notify")]
        module.register_subscription(
            SUBSCRIBE_NETWORK_EVENTS_ENDPOINT,
            SUBSCRIBE_NETWORK_EVENTS_ENDPOINT,
            UNSUBSCRIBE_NETWORK_EVENTS_ENDPOINT,
            |_, pending, ctx| async move {
                let sink = pending.accept().await?;
                let rx = ctx.evt_notifier.inner().subscribe();
                let stream = BroadcastStream::new(rx);
                Self::handle_event_subscription(
                    sink,
                    stream,
                    SUBSCRIBE_NETWORK_EVENTS_ENDPOINT.to_string(),
                )
                .await?;
                Ok(())
            },
        )?;

        #[cfg(feature = "websocket-notify")]
        module.register_subscription(
            SUBSCRIBE_RUN_WORKFLOW_ENDPOINT,
            SUBSCRIBE_RUN_WORKFLOW_ENDPOINT,
            UNSUBSCRIBE_RUN_WORKFLOW_ENDPOINT,
            |params, pending, ctx| async move {
                match params.one::<listener::Run<'_>>() {
                    Ok(listener::Run { name, workflow }) => {
                        let (tx, rx) = oneshot::channel();
                        ctx.runner_sender
                            .send((Message::RunWorkflow((name, workflow)), Some(tx)))
                            .await?;

                        if let Ok(Ok(Message::AckWorkflow((cid, name)))) =
                            time::timeout_at(Instant::now() + ctx.receiver_timeout, rx).await
                        {
                            let sink = pending.accept().await?;
                            ctx.workflow_listeners
                                .insert(sink.subscription_id(), (cid, name));
                            let rx = ctx.workflow_msg_notifier.inner().subscribe();
                            let stream = BroadcastStream::new(rx);
                            Self::handle_workflow_subscription(sink, stream, ctx).await?;
                        } else {
                            warn!("did not acknowledge message in time");
                            let _ = pending
                                .reject(ErrorObjectOwned::from(ErrorObjectOwned::from(
                                    ErrorCode::InternalError,
                                )))
                                .await;
                        }
                    }
                    Err(err) => {
                        warn!("failed to parse run workflow params: {}", err);
                        let _ = pending.reject(err).await;
                    }
                }
                Ok(())
            },
        )?;

        Ok(module)
    }

    #[cfg(feature = "websocket-notify")]
    async fn handle_event_subscription(
        mut sink: SubscriptionSink,
        mut stream: BroadcastStream<notifier::Message>,
        subscription_type: String,
    ) -> Result<()> {
        let rt_hdl = Handle::current();
        rt_hdl.spawn(async move {
            loop {
                select! {
                    _ = sink.closed() => {
                        break Ok(());
                    }
                    next_msg = stream.next() => {
                        let msg = match next_msg {
                            Some(Ok(notifier::Message {
                                header: Header {
                                    subscription: SubscriptionTyp::EventSub(evt),
                                    ..
                                },
                                payload,
                            })) if evt == subscription_type => payload,
                            Some(Ok(_)) => continue,
                            Some(Err(err)) => {
                                error!("subscription stream error: {}", err);
                                break Err(err.into());
                            }
                            None => break Ok(()),
                        };
                        let sub_msg = SubscriptionMessage::from_json(&msg)?;
                        match sink.try_send(sub_msg) {
                            Ok(()) => (),
                            Err(TrySendError::Closed(_)) => {
                                break Err(anyhow!("subscription sink closed"));
                            }
                            Err(TrySendError::Full(_)) => {
                                info!("subscription sink full");
                            }
                        }
                    }
                }
            }
        });

        Ok(())
    }

    #[cfg(feature = "websocket-notify")]
    async fn handle_workflow_subscription(
        mut sink: SubscriptionSink,
        mut stream: BroadcastStream<notifier::Message>,
        ctx: Arc<Context>,
    ) -> Result<()> {
        let rt_hdl = Handle::current();
        rt_hdl.spawn(async move {
        loop {
            select! {
                _ = sink.closed() => {
                    ctx.workflow_listeners.remove(&sink.subscription_id());
                    break Ok(());
                }
                next_msg = stream.next() => {
                    let msg = match next_msg {
                        Some(Ok(notifier::Message {
                            header: Header { subscription: SubscriptionTyp::Cid(cid), ident },
                            payload,
                        })) => {
                            let msg = ctx.workflow_listeners
                                .get(&sink.subscription_id())
                                .and_then(|v| {
                                    let (v_cid, v_name) = v.value();
                                    if v_cid == &cid && (Some(v_name) == ident.as_ref() || ident.is_none()) {
                                        Some(payload)
                                    } else {
                                        None
                                    }
                                });
                            msg
                        }
                        Some(Ok(notifier::Message {
                            header: notifier::Header { subscription: _sub, ..},
                            ..
                        })) => {
                            continue;
                        }
                        Some(Err(err)) => {
                            error!("subscription stream error: {}", err);
                            ctx.workflow_listeners.remove(&sink.subscription_id());
                            break Err(err.into());
                        }
                        None => break Ok(()),
                    };

                    if let Some(msg) = msg {
                        let sub_msg = SubscriptionMessage::from_json(&msg)?;
                        match sink.try_send(sub_msg) {
                            Ok(()) => (),
                            Err(TrySendError::Closed(_)) => {
                                ctx.workflow_listeners.remove(&sink.subscription_id());
                                break Err(anyhow!("subscription sink closed"));
                            }
                            Err(TrySendError::Full(_)) => {
                                info!("subscription sink full");
                            }
                        }
                    }
                }
            }
        }
    });

        Ok(())
    }
}
