use std::sync::Arc;
use actix::{Actor, AsyncContext, ActorContext, Handler, StreamHandler};
use datasource::{Context, RemoteGraphQLDataSource};
use graphgate_schema::ComposedSchema;
use actix_web_actors::ws;
use actix_web_actors::ws::{CloseCode, CloseReason, Message, ProtocolError};
use value::ConstValue;
use graphgate_planner::{Response, ServerError};
use crate::ServiceRouteTable;
use crate::websocket::protocol::{ClientMessage, ConnectionError, ServerMessage};
use crate::websocket::{Protocols, WebSocketController, grouped_stream::StreamEvent};

pub struct Subscription<S: RemoteGraphQLDataSource> {
    schema: Arc<ComposedSchema>,
    route_table: Arc<ServiceRouteTable<S>>,
    context: Arc<Context>,
    controller: Option<WebSocketController>,
    protocol: Protocols
}

impl<S: RemoteGraphQLDataSource> Subscription<S> {
    pub fn new(schema: Arc<ComposedSchema>, route_table: Arc<ServiceRouteTable<S>>, context: Arc<Context>, protocol: Protocols) -> Self {
        let controller = None;
        Self {
            schema,
            route_table,
            context,
            controller,
            protocol
        }
    }
}

impl<S: RemoteGraphQLDataSource> Actor for Subscription<S> {
    type Context = ws::WebsocketContext<Self>;
}

impl<S: RemoteGraphQLDataSource> StreamHandler<Result<ws::Message, ws::ProtocolError>> for Subscription<S> {
    fn handle(&mut self, item: Result<Message, ProtocolError>, ctx: &mut Self::Context) {
        if let Ok(Message::Text(ref text)) = item {
            let bytes = text.as_bytes();
            let client_msg = match serde_json::from_slice::<ClientMessage>(bytes) {
                Ok(client_msg) => client_msg,
                Err(_) => return,
            };
            match client_msg {
                ClientMessage::ConnectionInit { payload } if self.controller.is_none() => {
                    let context = Arc::clone(&self.context);
                    self.controller = Some(WebSocketController::new(self.route_table.clone(), payload, context));
                    let message = serde_json::to_string(&ServerMessage::ConnectionAck).unwrap();
                    ctx.text(message);
                }
                ClientMessage::ConnectionInit { .. } => {
                    match self.protocol {
                        Protocols::SubscriptionsTransportWS => {
                            let err_msg =
                                serde_json::to_string(&ServerMessage::ConnectionError {
                                    payload: ConnectionError {
                                        message: "Too many initialisation requests.",
                                    },
                                }).unwrap();
                            ctx.text(err_msg);
                            ctx.stop();
                        }
                        Protocols::GraphQLWS => {
                            let reason = CloseReason::from(CloseCode::Unsupported);
                            ctx.close(Some(reason));
                            ctx.stop();
                        }
                    }
                }
                ClientMessage::Stop { id } => {
                    let table = self.route_table.clone();
                    let context = Arc::clone(&self.context);
                    let controller = self.controller.get_or_insert_with(|| WebSocketController::new(table, None, context)).clone();
                    let id = id.to_owned();
                    actix::spawn(async move {
                        controller.stop(id).await
                    });
                }
                ClientMessage::Start { id, payload } | ClientMessage::Subscribe { id, payload } => {
                    let table = self.route_table.clone();
                    let context = Arc::clone(&self.context);
                    let controller = self.controller.get_or_insert_with(|| WebSocketController::new(table, None, context)).clone();
                    let document = match parser::parse_query(&payload.query) {
                        Ok(document) => document,
                        Err(err) => {
                            let resp = Response {
                                data: ConstValue::Null,
                                errors: vec![ServerError::new(err.to_string())],
                                extensions: Default::default(),
                                headers: Default::default()
                            };
                            let data = ServerMessage::Data { id, payload: resp };
                            let message = serde_json::to_string(&data).unwrap();
                            ctx.text(message);
                            let complete = ServerMessage::Complete { id };
                            let message = serde_json::to_string(&complete).unwrap();
                            ctx.text(message);
                            ctx.stop();
                            return;
                        }
                    };
                    let id = Arc::new(id.to_string());
                    let schema = self.schema.clone();
                    let stream = {
                        let id = id.clone();
                        use graphgate_planner::PlanBuilder;
                        use crate::executor::Executor;
                        async_stream::stream! {
                            let builder = PlanBuilder::new(&schema, document).variables(payload.variables);
                            let node = match builder.plan() {
                                Ok(node) => node,
                                Err(resp) => {
                                    yield StreamEvent::Data(Arc::clone(&id), resp);
                                    yield StreamEvent::Complete(id);
                                    return;
                                }
                            };
                            let executor = Executor::new(&schema);
                            let mut stream = executor.execute_stream(controller.clone(), &id, &node).await;
                            use futures_util::StreamExt;
                            while let Some(item) = stream.next().await {
                                yield StreamEvent::Data(Arc::clone(&id), item);
                            }
                            yield StreamEvent::Complete(id);
                        }
                    };
                    ctx.add_message_stream(stream);
                }
                _ => {}
            }
        }
        if let Ok(Message::Close(_)) | Err(_) = item {
            ctx.stop();
        }
    }
}
type Event = StreamEvent<Arc<std::string::String>, graphgate_planner::Response>;

impl<S: RemoteGraphQLDataSource> Handler<Event> for Subscription<S> {
    type Result = ();
    fn handle(&mut self, msg: Event, ctx: &mut Self::Context) -> Self::Result {
        match msg {
            StreamEvent::Data(id, resp) => {
                let data = self.protocol.next_message(&id, resp);
                let message = serde_json::to_string(&data).unwrap();
                ctx.text(message);
            }
            StreamEvent::Complete(id) => {
                let complete = ServerMessage::Complete { id: &id };
                let message = serde_json::to_string(&complete).unwrap();
                ctx.text(message);
            }
        }
    }
}