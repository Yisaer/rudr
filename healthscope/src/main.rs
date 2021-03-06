use chrono::{DateTime, Utc};
use clap::{App, Arg};
use env_logger;
use failure::{format_err, Error};
use futures::task::{current, Task};
use futures::{future, Async};
use hyper::rt::Future;
use hyper::service::{service_fn, service_fn_ok};
use hyper::{Body, Method, Request, Response, Server, StatusCode};
use kube::api::{ListParams, ObjectList, RawApi};
use kube::{client::APIClient, config::incluster_config, config::load_kube_config};
use log::{debug, error, info};
use rudr::instigator::{combine_name, CONFIG_GROUP, CONFIG_VERSION};
use rudr::schematic::component_instance::KubeComponentInstance;
use rudr::schematic::scopes::health::{
    ComponentInfo, HealthScopeObject, HealthStatus, HEALTH_SCOPE_CRD, HEALTH_SCOPE_GROUP,
    HEALTH_SCOPE_VERSION,
};
use std::{
    sync::{Arc, Mutex},
    thread,
};

const DEFAULT_NAMESPACE: &str = "default";
const DEFAULT_PROBE_INTERVAL: i64 = 30;

fn kubeconfig() -> kube::Result<kube::config::Configuration> {
    // If env var is set, use in cluster config
    if std::env::var("KUBERNETES_PORT").is_ok() {
        return incluster_config();
    }
    load_kube_config()
}

fn main() -> Result<(), Error> {
    let flags = App::new("healthscope")
        .version(env!("CARGO_PKG_VERSION"))
        .arg(
            Arg::with_name("metrics-addr")
                .short("m")
                .long("metrics-addr")
                .default_value(":8080")
                .help("The address the metric endpoint binds to."),
        )
        .arg(
            Arg::with_name("addr")
                .short("p")
                .long("endpoint-address")
                .default_value(":80")
                .help("The address the health scope endpoint binds to."),
        )
        .get_matches();
    let metrics_addr = "0.0.0.0".to_owned() + flags.value_of("metrics-addr").unwrap();
    let endpoint_addr = "0.0.0.0".to_owned() + flags.value_of("addr").unwrap();

    env_logger::init();
    info!("starting server");

    let top_ns = std::env::var("KUBERNETES_NAMESPACE").unwrap_or_else(|_| DEFAULT_NAMESPACE.into());
    let top_cfg = kubeconfig().expect("Load default kubeconfig");

    let cfg_watch = top_cfg.clone();

    let health_scope_watch = std::thread::spawn(move || {
        let ns = top_ns.clone();
        let healthscope_resource = RawApi::customResource("healthscopes")
            .version("v1alpha1")
            .group("core.oam.dev")
            .within(ns.as_str());
        let client = APIClient::new(cfg_watch);
        let mut cnt = 0;
        loop {
            let req = healthscope_resource.list(&ListParams::default())?;
            match client.request::<ObjectList<HealthScopeObject>>(req) {
                Ok(health_scopes) => {
                    for scope in health_scopes.items {
                        if let Err(res) = aggregate_component_health(&client, scope, ns.clone()) {
                            // Log the error and continue.
                            error!("Error processing event: {:?}", res)
                        };
                    }
                }
                Err(e) => error!("get health scope list err {:?}", e),
            }
            cnt = (cnt + 1) % 10;
            if cnt == 0 {
                debug!("health scope aggregate loop running...");
            }
            //FIXME: we could change this to use an informer if we have a runtime controller queue
            std::thread::sleep(std::time::Duration::from_secs(5));
        }
    });

    let server = std::thread::spawn(move || {
        let addr = endpoint_addr.parse().unwrap();
        info!("Server is running on {}", addr);
        hyper::rt::run(
            Server::bind(&addr)
                .serve(move || service_fn(serve_health))
                .map_err(|e| eprintln!("server error: {}", e)),
        );
    });

    std::thread::spawn(move || {
        let addr = metrics_addr.parse().unwrap();
        info!("Health server is running on {}", addr);
        hyper::rt::run(
            Server::bind(&addr)
                .serve(|| {
                    service_fn_ok(|_req| match (_req.method(), _req.uri().path()) {
                        (&Method::GET, "/health") => {
                            debug!("health check");
                            Response::new(Body::from("OK"))
                        }
                        _ => Response::builder()
                            .status(StatusCode::NOT_FOUND)
                            .body(Body::from(""))
                            .unwrap(),
                    })
                })
                .map_err(|e| eprintln!("health server error: {}", e)),
        );
    })
    .join()
    .unwrap();

    server.join().unwrap();
    health_scope_watch.join().unwrap()
}

pub struct HealthFuture {
    shared_state: Arc<Mutex<SharedState>>,
}

/// Shared state between the future and the waiting thread
struct SharedState {
    /// Whether or not the sleep time has elapsed
    completed: bool,
    resp: String,
    task: Option<Task>,
}

impl Future for HealthFuture {
    type Item = Response<Body>;
    type Error = hyper::Error;
    fn poll(&mut self) -> futures::Poll<Response<Body>, hyper::Error> {
        // Look at the shared state to see if the timer has already completed.
        let mut shared_state = self.shared_state.lock().unwrap();
        if shared_state.completed {
            Ok(Async::Ready(Response::new(Body::from(
                shared_state.resp.clone(),
            ))))
        } else {
            shared_state.task = Some(current());
            Ok(Async::NotReady)
        }
    }
}

// FIXME kube-rs client doesn't support async call so we have to wrap it in HealthFuture here.
// we could remove this wrapper until kube-rs support. https://github.com/clux/kube-rs/issues/63
impl HealthFuture {
    /// Create a new `TimerFuture` which will complete after the provided
    /// timeout.
    pub fn new(instance: String) -> Self {
        let shared_state = Arc::new(Mutex::new(SharedState {
            completed: false,
            task: None,
            resp: String::new(),
        }));

        // Spawn the new thread
        let thread_shared_state = shared_state.clone();
        thread::spawn(move || {
            let res = match request_health(instance) {
                Ok(status) => status.clone(),
                Err(err) => {
                    error!("{:?}", err);
                    format!("{}", err)
                }
            };

            let mut shared_state = thread_shared_state.lock().unwrap();
            // Signal that the request has completed and wake up the last
            // task on which the future was polled, if one exists.
            shared_state.completed = true;
            shared_state.resp = res;
            if let Some(ref task) = shared_state.task {
                task.notify()
            }
        });

        HealthFuture { shared_state }
    }
}

type BoxFut = Box<dyn Future<Item = Response<Body>, Error = hyper::Error> + Send>;

// serve_health make health scope controller as an http server, it will serve requests and get the real health status from health scope instance
fn serve_health(req: Request<Body>) -> BoxFut {
    let mut response = Response::new(Body::empty());
    let path = req.uri().path().to_owned();
    match (req.method(), path) {
        (&Method::GET, path) => {
            let instance = path.trim_start_matches('/').to_string();
            info!("{} health scope requested", instance);
            return Box::new(HealthFuture::new(instance));
        }
        _ => *response.status_mut() = StatusCode::NOT_FOUND,
    }
    Box::new(future::ok(response))
}

// request_health will request health scope instance CR and get status from the CR object
fn request_health(instance_name: String) -> Result<String, Error> {
    let namespace =
        std::env::var("KUBERNETES_NAMESPACE").unwrap_or_else(|_| DEFAULT_NAMESPACE.into());
    let cfg = kubeconfig().unwrap();
    println!(
        "cfg {:?}, instance {}",
        cfg.base_path.clone(),
        instance_name
    );
    let client = &(APIClient::new(cfg));
    println!("client namespace {}", namespace.clone());
    let healthscope_resource = RawApi::customResource("healthscopes")
        .version("v1alpha1")
        .group("core.oam.dev")
        .within(namespace.as_str());
    let req = healthscope_resource.get(instance_name.as_str())?;
    let obj = client.request::<HealthScopeObject>(req)?;
    let mut health = "healthy";
    obj.status.map(|status| {
        status.clone().components.map(|comps| {
            comps.iter().for_each(|c| {
                if let Some(real_status) = c.status.as_ref() {
                    if real_status != "healthy" {
                        health = "unhealthy"
                    }
                };
            })
        })
    });
    Ok(health.to_string())
}

fn aggregate_component_health(
    client: &APIClient,
    mut event: HealthScopeObject,
    namespace: String,
) -> Result<(), Error> {
    let interval = event.spec.probe_interval.unwrap_or(DEFAULT_PROBE_INTERVAL);
    if !time_to_aggregate(event.status.clone(), interval) {
        return Ok(());
    }
    info!("start to probe instance: {}", event.metadata.name);
    match (
        event.spec.probe_method.as_str(),
        event.spec.probe_endpoint.as_str(),
    ) {
        ("kube-get", ".status") => {
            let components =
                event
                    .status
                    .and_then(|status| status.components)
                    .and_then(|mut components| {
                        for c in components.iter_mut() {
                            c.status = Some(get_health_from_component(
                                client,
                                c.clone(),
                                namespace.clone(),
                            ))
                        }
                        Some(components)
                    });
            event.status = Some(HealthStatus {
                components,
                last_aggregate_timestamp: Some(Utc::now().to_rfc3339()),
            });
            let pp = kube::api::PatchParams::default();
            let healthscope_resource = RawApi::customResource(HEALTH_SCOPE_CRD)
                .version(HEALTH_SCOPE_VERSION)
                .group(HEALTH_SCOPE_GROUP)
                .within(namespace.as_str());
            let req = healthscope_resource.patch(
                event.metadata.clone().name.as_str(),
                &pp,
                serde_json::to_vec(&event)?,
            )?;
            client.request::<HealthScopeObject>(req)?;
            Ok(())
        }
        _ => Err(format_err!(
            "unknown probe-method {} and probe_endpoint {}",
            event.spec.probe_method,
            event.spec.probe_endpoint
        )),
    }
}

fn get_health_from_component(client: &APIClient, info: ComponentInfo, namespace: String) -> String {
    let name = combine_name(info.name, info.instance_name);
    let crd_req = RawApi::customResource("componentinstances")
        .group(CONFIG_GROUP)
        .version(CONFIG_VERSION)
        .within(namespace.as_str());
    let req = crd_req.get(name.as_str()).unwrap();
    let res: KubeComponentInstance = match client.request(req) {
        Ok(ins) => ins,
        Err(e) => {
            error!("get component instance failed {:?}", e);
            return "unhealthy".to_string();
        }
    };
    res.status.unwrap_or_else(|| "unhealthy".to_string())
}

fn time_to_aggregate(status: Option<HealthStatus>, interval: i64) -> bool {
    if interval <= 0 {
        return true;
    }
    if status.is_none() || status.clone().unwrap().last_aggregate_timestamp.is_none() {
        return true;
    }
    let last_aggregate_time = status.unwrap().last_aggregate_timestamp.unwrap();
    let last_time = match DateTime::parse_from_rfc3339(last_aggregate_time.as_str()) {
        Ok(last) => last,
        Err(e) => {
            error!("parse last time err {:?}", e);
            return true;
        }
    };
    let sys_time = Utc::now();
    let duration = sys_time.signed_duration_since(last_time);
    if duration.num_seconds() >= interval {
        return true;
    }
    false
}

#[cfg(test)]
mod test {
    use crate::time_to_aggregate;
    use chrono::{Duration, Utc};
    use rudr::schematic::scopes::health::HealthStatus;

    #[test]
    fn test_time_to_action() {
        assert_eq!(time_to_aggregate(None, 10), true);
        let status = Some(HealthStatus {
            last_aggregate_timestamp: None,
            ..Default::default()
        });
        assert_eq!(time_to_aggregate(status, 10), true);
        let status = Some(HealthStatus {
            last_aggregate_timestamp: Some(
                Utc::now()
                    .checked_sub_signed(Duration::seconds(11))
                    .unwrap()
                    .to_rfc3339(),
            ),
            ..Default::default()
        });
        assert_eq!(time_to_aggregate(status.clone(), 10), true);
        assert_eq!(time_to_aggregate(status.clone(), 15), false);
        assert_eq!(time_to_aggregate(status.clone(), 0), true);
    }
}
