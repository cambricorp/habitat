use crate::manager::{self,
                     service::{HealthCheckHook,
                               HealthCheckResult}};
use actix;
use actix_web::{http::{self,
                       StatusCode},
                middleware::{Finished,
                             Middleware,
                             Started},
                pred::Predicate,
                server,
                App,
                FromRequest,
                HttpRequest,
                HttpResponse,
                Path,
                Request};
use habitat_common::{self,
                     templating::hooks,
                     types::HttpListenAddr,
                     FeatureFlag};
use habitat_core::{crypto,
                   env as henv,
                   service::ServiceGroup};
use prometheus::{self,
                 CounterVec,
                 Encoder,
                 HistogramTimer,
                 HistogramVec,
                 TextEncoder};
use rustls::ServerConfig;
use serde_json::{self,
                 Value as Json};
use std::{self,
          cell::Cell,
          fs::File,
          io::Read,
          sync::{Arc,
                 Condvar,
                 Mutex,
                 RwLock},
          thread};

const APIDOCS: &str = include_str!(concat!(env!("OUT_DIR"), "/api.html"));
pub const HTTP_THREADS_ENVVAR: &str = "HAB_SUP_HTTP_THREADS";
pub const HTTP_THREAD_COUNT: usize = 2;

/// Default listening port for the HTTPGateway listener.
pub const DEFAULT_PORT: u16 = 9631;

lazy_static! {
    static ref HTTP_GATEWAY_REQUESTS: CounterVec =
        register_counter_vec!("hab_sup_http_gateway_requests_total",
                              "Total number of HTTP gateway requests",
                              &["path"]).unwrap();
    static ref HTTP_GATEWAY_REQUEST_DURATION: HistogramVec =
        register_histogram_vec!("hab_sup_http_gateway_request_duration_seconds",
                                "The latency for HTTP gateway requests",
                                &["path"]).unwrap();
}

habitat_core::env_config!(
    /// This represents an environment variable that holds an authentication token for the supervisor's
    /// HTTP gateway. If the environment variable is present, then its value is the auth token and all
    /// of the HTTP endpoints will require its presence. If it's not present, then everything continues
    /// to work unauthenticated.
    #[derive(Clone, Debug)]
    pub GatewayAuthenticationToken,
    Option<String>,
    HAB_SUP_GATEWAY_AUTH_TOKEN,
    None,
    std::string::ParseError,
    s,
    Ok(GatewayAuthenticationToken(Some(String::from(s)))));

#[derive(Default, Serialize)]
struct HealthCheckBody {
    status: String,
    stdout: String,
    stderr: String,
}

impl Into<StatusCode> for HealthCheckResult {
    fn into(self) -> StatusCode {
        match self {
            HealthCheckResult::Ok | HealthCheckResult::Warning => StatusCode::OK,
            HealthCheckResult::Critical => StatusCode::SERVICE_UNAVAILABLE,
            HealthCheckResult::Unknown => StatusCode::INTERNAL_SERVER_ERROR,
        }
    }
}

struct AppState {
    gateway_state:        Arc<RwLock<manager::GatewayState>>,
    authentication_token: Option<String>,
    timer:                Cell<Option<HistogramTimer>>,
    feature_flags:        FeatureFlag,
}

impl AppState {
    fn new(gs: Arc<RwLock<manager::GatewayState>>,
           authentication_token: GatewayAuthenticationToken,
           feature_flags: FeatureFlag)
           -> Self {
        AppState { gateway_state: gs,
                   // We'll unwrap to the inner type, since the
                   // GatewayAuthenticationToken type has done its job by this point.
                   authentication_token: authentication_token.0,
                   timer: Cell::new(None),
                   feature_flags }
    }
}

// Begin middleware
struct Authentication;

impl Middleware<AppState> for Authentication {
    fn start(&self, req: &HttpRequest<AppState>) -> actix_web::Result<Started> {
        let current_token = req.state().authentication_token.as_ref();
        let current_token = match current_token {
            Some(t) => t,
            None => {
                debug!("No authentication token present. HTTP gateway starting in \
                        unauthenticated mode.");
                return Ok(Started::Done);
            }
        };

        // From this point forward, we know that we have an
        // authentication token in the state. Therefore, anything
        // short of a fully formed Authorization header (yes,
        // Authorization; HTTP is fun, kids!) containing a Bearer
        // token that matches the value we have in our state, results
        // in an Unauthorized response.
        let hdr = match req.headers()
                           .get(http::header::AUTHORIZATION)
                           .ok_or("header missing")
                           .and_then(|hv| hv.to_str().or(Err("can't convert to str")))
        {
            Ok(h) => h,
            Err(e) => {
                debug!("Error reading required Authorization header: {:?}.", e);
                return Ok(Started::Response(HttpResponse::Unauthorized().finish()));
            }
        };

        let hdr_components: Vec<&str> = hdr.split_whitespace().collect();

        match hdr_components.as_slice() {
            ["Bearer", incoming_token] if crypto::secure_eq(current_token, incoming_token) => {
                Ok(Started::Done)
            }
            _ => Ok(Started::Response(HttpResponse::Unauthorized().finish())),
        }
    }
}

struct Metrics;

impl Middleware<AppState> for Metrics {
    fn start(&self, req: &HttpRequest<AppState>) -> actix_web::Result<Started> {
        let label_values = &[req.path()];

        HTTP_GATEWAY_REQUESTS.with_label_values(label_values).inc();
        let timer = HTTP_GATEWAY_REQUEST_DURATION.with_label_values(label_values)
                                                 .start_timer();
        req.state().timer.set(Some(timer));

        Ok(Started::Done)
    }

    fn finish(&self, req: &HttpRequest<AppState>, _resp: &HttpResponse) -> Finished {
        let timer = req.state().timer.replace(None);

        if timer.is_some() {
            timer.unwrap().observe_duration();
        }

        Finished::Done
    }
}

// End middleware

#[derive(Debug, PartialEq, Eq)]
pub enum ServerStartup {
    NotStarted,
    Started,
    BindFailed,
}

pub struct Server;

impl Server {
    pub fn run(listen_addr: HttpListenAddr,
               tls_config: Option<ServerConfig>,
               gateway_state: Arc<RwLock<manager::GatewayState>>,
               authentication_token: GatewayAuthenticationToken,
               feature_flags: FeatureFlag,
               control: Arc<(Mutex<ServerStartup>, Condvar)>) {
        thread::spawn(move || {
            let &(ref lock, ref cvar) = &*control;
            let sys = actix::System::new("sup-http-gateway");
            let thread_count = match henv::var(HTTP_THREADS_ENVVAR) {
                Ok(val) => {
                    match val.parse::<usize>() {
                        Ok(v) => v,
                        Err(_) => HTTP_THREAD_COUNT,
                    }
                }
                Err(_) => HTTP_THREAD_COUNT,
            };

            let mut server = server::new(move || {
                                 let app_state = AppState::new(gateway_state.clone(),
                                                               authentication_token.clone(),
                                                               feature_flags);
                                 App::with_state(app_state).middleware(Authentication)
                                                           .middleware(Metrics)
                                                           .configure(routes)
                             }).workers(thread_count);

            // On Windows the default actix signal handler will create a ctrl+c handler for the
            // process which will disable default windows ctrl+c behavior and allow us to
            // handle via check_for_signal in the supervisor service loop. However, if the
            // supervisor is in a long running non-run hook, that loop will not get to
            // check_for_signal in a reasonable amount of time and the supervisor will not
            // respond to ctrl+c. On Windows, we let the launcher catch ctrl+c and gracefully
            // shut down services. ctrl+c should simply halt the supervisor. The IgnoreSignals
            // feature is always enabled in the Habitat Windows Service which relies on ctrl+c
            // signals to stop the supervisor.
            if feature_flags.contains(FeatureFlag::IGNORE_SIGNALS) {
                server = server.disable_signals();
            }

            let bind = match tls_config {
                Some(c) => server.bind_rustls(listen_addr.to_string(), c),
                None => server.bind(listen_addr.to_string()),
            };

            *lock.lock().expect("Control mutex is poisoned") = match bind {
                Ok(b) => {
                    b.start();
                    ServerStartup::Started
                }
                Err(e) => {
                    error!("HTTP gateway failed to bind: {}", e);
                    ServerStartup::BindFailed
                }
            };

            cvar.notify_one();

            sys.run();
        });
    }
}

struct RedactHTTP;

impl Predicate<AppState> for RedactHTTP {
    fn check(&self, _req: &Request, state: &AppState) -> bool {
        !state.feature_flags.contains(FeatureFlag::REDACT_HTTP)
    }
}

fn routes(app: App<AppState>) -> App<AppState> {
    app.resource("/", |r| r.get().f(doc))
       .resource("/services", |r| r.get().f(services))
       .resource("/services/{svc}/{group}", |r| {
           r.get().f(service_without_org)
       })
       .resource("/services/{svc}/{group}/config", |r| {
           r.get().f(config_without_org)
       })
       .resource("/services/{svc}/{group}/health", |r| {
           r.get().f(health_without_org)
       })
       .resource("/services/{svc}/{group}/{org}", |r| {
           r.get().f(service_with_org)
       })
       .resource("/services/{svc}/{group}/{org}/config", |r| {
           r.get().f(config_with_org)
       })
       .resource("/services/{svc}/{group}/{org}/health", |r| {
           r.get().f(health_with_org)
       })
       .resource("/butterfly", |r| r.get().filter(RedactHTTP).f(butterfly))
       .resource("/census", |r| r.get().filter(RedactHTTP).f(census))
       .resource("/metrics", |r| r.get().f(metrics))
}

fn json_response(data: String) -> HttpResponse {
    HttpResponse::Ok().content_type("application/json")
                      .body(data)
}

// Begin route handlers
fn butterfly(req: &HttpRequest<AppState>) -> HttpResponse {
    let data = &req.state()
                   .gateway_state
                   .read()
                   .expect("GatewayState lock is poisoned")
                   .butterfly_data;
    json_response(data.to_string())
}

fn census(req: &HttpRequest<AppState>) -> HttpResponse {
    let data = &req.state()
                   .gateway_state
                   .read()
                   .expect("GatewayState lock is poisoned")
                   .census_data;
    json_response(data.to_string())
}

fn services(req: &HttpRequest<AppState>) -> HttpResponse {
    let data = &req.state()
                   .gateway_state
                   .read()
                   .expect("GatewayState lock is poisoned")
                   .services_data;
    json_response(data.to_string())
}

// Honestly, this doesn't feel great, but it's the pattern builder-api uses, and at the
// moment, I don't have a better way of doing it.
fn config_with_org(req: &HttpRequest<AppState>) -> HttpResponse {
    let (svc, group, org) = Path::<(String, String, String)>::extract(&req).unwrap()
                                                                           .into_inner();
    config(req, svc, group, Some(&org))
}

fn config_without_org(req: &HttpRequest<AppState>) -> HttpResponse {
    let (svc, group) = Path::<(String, String)>::extract(&req).unwrap()
                                                              .into_inner();
    config(req, svc, group, None)
}

fn config(req: &HttpRequest<AppState>,
          svc: String,
          group: String,
          org: Option<&str>)
          -> HttpResponse {
    let data = &req.state()
                   .gateway_state
                   .read()
                   .expect("GatewayState lock is poisoned")
                   .services_data;
    let service_group = match ServiceGroup::new(None, svc, group, org) {
        Ok(sg) => sg,
        Err(_) => return HttpResponse::BadRequest().finish(),
    };

    match service_from_services(&service_group, &data) {
        Some(mut s) => HttpResponse::Ok().json(s["cfg"].take()),
        None => HttpResponse::NotFound().finish(),
    }
}

fn health_with_org(req: &HttpRequest<AppState>) -> HttpResponse {
    let (svc, group, org) = Path::<(String, String, String)>::extract(&req).unwrap()
                                                                           .into_inner();
    health(req, svc, group, Some(&org))
}

fn health_without_org(req: &HttpRequest<AppState>) -> HttpResponse {
    let (svc, group) = Path::<(String, String)>::extract(&req).unwrap()
                                                              .into_inner();
    health(req, svc, group, None)
}

fn health(req: &HttpRequest<AppState>,
          svc: String,
          group: String,
          org: Option<&str>)
          -> HttpResponse {
    let service_group = match ServiceGroup::new(None, svc, group, org) {
        Ok(sg) => sg,
        Err(_) => return HttpResponse::BadRequest().finish(),
    };

    let gateway_state = &req.state()
                            .gateway_state
                            .read()
                            .expect("GatewayState lock is poisoned");
    let health_check = gateway_state.health_check_data.get(&service_group);

    if health_check.is_some() {
        let mut body = HealthCheckBody::default();
        let stdout_path = hooks::stdout_log_path::<HealthCheckHook>(&service_group);
        let stderr_path = hooks::stderr_log_path::<HealthCheckHook>(&service_group);
        let http_status: StatusCode = health_check.unwrap().clone().into();

        body.status = health_check.unwrap().to_string();
        if let Ok(mut file) = File::open(&stdout_path) {
            let _ = file.read_to_string(&mut body.stdout);
        }
        if let Ok(mut file) = File::open(&stderr_path) {
            let _ = file.read_to_string(&mut body.stderr);
        }

        HttpResponse::build(http_status).json(&body)
    } else {
        debug!("Didn't find any health data for service group {:?}",
               &service_group);
        HttpResponse::NotFound().finish()
    }
}

fn service_with_org(req: &HttpRequest<AppState>) -> HttpResponse {
    let (svc, group, org) = Path::<(String, String, String)>::extract(&req).unwrap()
                                                                           .into_inner();
    service(req, svc, group, Some(&org))
}

fn service_without_org(req: &HttpRequest<AppState>) -> HttpResponse {
    let (svc, group) = Path::<(String, String)>::extract(&req).unwrap()
                                                              .into_inner();
    service(req, svc, group, None)
}

fn service(req: &HttpRequest<AppState>,
           svc: String,
           group: String,
           org: Option<&str>)
           -> HttpResponse {
    let data = &req.state()
                   .gateway_state
                   .read()
                   .expect("GatewayState lock is poisoned")
                   .services_data;
    let service_group = match ServiceGroup::new(None, svc, group, org) {
        Ok(sg) => sg,
        Err(_) => return HttpResponse::BadRequest().finish(),
    };

    match service_from_services(&service_group, &data) {
        Some(s) => HttpResponse::Ok().json(s),
        None => HttpResponse::NotFound().finish(),
    }
}

fn metrics(_req: &HttpRequest<AppState>) -> HttpResponse {
    let encoder = TextEncoder::new();
    let metric_families = prometheus::gather();
    let mut buffer = vec![];

    if let Err(e) = encoder.encode(&metric_families, &mut buffer) {
        error!("Error encoding metrics: {:?}", e);
    }

    let resp = match String::from_utf8(buffer) {
        Ok(s) => s,
        Err(e) => {
            error!("Error constructing string from metrics buffer: {:?}", e);
            String::from("")
        }
    };

    HttpResponse::Ok().content_type(encoder.format_type())
                      .body(resp)
}

fn doc(_req: &HttpRequest<AppState>) -> HttpResponse {
    HttpResponse::Ok().content_type("text/html").body(APIDOCS)
}
// End route handlers

fn service_from_services(service_group: &ServiceGroup, services_json: &str) -> Option<Json> {
    match serde_json::from_str(services_json) {
        Ok(Json::Array(services)) => {
            services.into_iter()
                    .find(|s| s["service_group"] == service_group.as_ref())
        }
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use crate::test_helpers::*;
    use habitat_butterfly::{member::Member,
                            server::{Server,
                                     ServerProxy,
                                     Suitability},
                            trace::Trace};
    use serde_json;
    use std::{fs::File,
              io::Read,
              net::{IpAddr,
                    Ipv4Addr,
                    SocketAddr},
              path::PathBuf,
              sync::Mutex};

    fn validate_sample_file_against_schema(name: &str, schema: &str) {
        let path = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("tests")
                                                            .join("fixtures")
                                                            .join("http-gateway")
                                                            .join(name);

        let mut f = File::open(path).unwrap_or_else(|_| panic!("could not open {}", &name));
        let mut json = String::new();
        f.read_to_string(&mut json)
         .unwrap_or_else(|_| panic!("could not read {}", &name));

        assert_valid(&json, schema);
    }

    #[test]
    fn sample_census_file_is_valid() {
        validate_sample_file_against_schema("sample-census-output.json",
                                            "http_gateway_census_schema.json");
    }

    #[test]
    fn trivial_census_failure() {
        let failure = validate_string(
            r#"{"census_groups": {}, "changed": false, "last_election_counter": "narf"}"#,
            "http_gateway_census_schema.json",
        );
        assert!(!failure.is_valid(),
                "Expected schema validation to fail, but it succeeded");
    }

    #[test]
    fn sample_butterfly_file_is_valid() {
        validate_sample_file_against_schema("sample-butterfly-output.json",
                                            "http_gateway_butterfly_schema.json");
    }

    #[test]
    fn trivial_butterfly_failure() {
        let failure = validate_string(r#"{"departure": {}, "election": {}, "member": {}, "service": false, "service_file": []}"#, "http_gateway_butterfly_schema.json");
        assert!(!failure.is_valid(),
                "Expected schema validation to fail, but it succeeded");
    }

    #[test]
    fn butterfly_server_proxy_is_valid() {
        lazy_static! {
            static ref SWIM_PORT: Mutex<u16> = Mutex::new(6666);
            static ref GOSSIP_PORT: Mutex<u16> = Mutex::new(7777);
        }

        #[derive(Debug)]
        struct ZeroSuitability;
        impl Suitability for ZeroSuitability {
            fn get(&self, _service_group: &str) -> u64 { 0 }
        }

        fn start_server() -> Server {
            let swim_port;
            {
                let mut swim_port_guard = SWIM_PORT.lock().expect("SWIM_PORT poisoned");
                swim_port = *swim_port_guard;
                *swim_port_guard += 1;
            }
            let swim_listen = SocketAddr::new(IpAddr::V4(Ipv4Addr::new(127, 0, 0, 1)), swim_port);
            let gossip_port;
            {
                let mut gossip_port_guard = GOSSIP_PORT.lock().expect("GOSSIP_PORT poisoned");
                gossip_port = *gossip_port_guard;
                *gossip_port_guard += 1;
            }
            let gossip_listen =
                SocketAddr::new(IpAddr::V4(Ipv4Addr::new(127, 0, 0, 1)), gossip_port);
            let mut member = Member::default();
            member.swim_port = swim_port;
            member.gossip_port = gossip_port;
            Server::new(swim_listen,
                        gossip_listen,
                        member,
                        Trace::default(),
                        None,
                        None,
                        None,
                        Box::new(ZeroSuitability)).unwrap()
        }

        let server = start_server();
        let proxy = ServerProxy::new(&server);
        let json = serde_json::to_string(&proxy).unwrap();
        assert_valid(&json, "http_gateway_butterfly_schema.json");
    }

    #[test]
    fn sample_services_with_cfg_file_is_valid() {
        validate_sample_file_against_schema("sample-services-with-cfg-output.json",
                                            "http_gateway_services_schema.json");
    }

    #[test]
    fn sample_services_without_cfg_file_is_valid() {
        validate_sample_file_against_schema("sample-services-without-cfg-output.json",
                                            "http_gateway_services_schema.json");
    }

    #[test]
    fn trivial_services_failure() {
        let failure = validate_string(r#"[{"lulz": true}]"#, "http_gateway_services_schema.json");
        assert!(!failure.is_valid(),
                "Expected schema validation to fail, but it succeeded");
    }
}
