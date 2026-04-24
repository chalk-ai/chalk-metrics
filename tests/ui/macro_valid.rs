mod tags {
    chalk_metrics::define_tags! {
        pub Status => "status" {
            Success => "success",
            Failure => "failure",
        }
    }

    chalk_metrics::define_tags! {
        pub Endpoint => "endpoint";
        pub Region => "region";
    }
}

mod namespaces {
    chalk_metrics::define_namespaces! {
        pub Http => "http";
    }

    chalk_metrics::define_namespaces! {
        pub Resolver => "resolver";
        pub HttpAuth(parent = Http) => "auth";
    }
}

mod http {
    use super::namespaces::{Http, HttpAuth};
    use super::tags::{Endpoint, Region, Status};

    chalk_metrics::define_metrics! {
        group(namespace = Http, tags = [Endpoint, Status]) {
            pub count HttpRequestCount => "request_count", "Total HTTP requests";
            pub histogram HttpRequestLatency => "request_latency", "HTTP request latency";
        }

        group(namespace = Http, tags = [Endpoint]) {
            pub gauge HttpActiveConnections => "active_connections",
                "Current active connections",
                tags += [
                    optional Region,
                ];
        }

        group(namespace = HttpAuth, tags = [Endpoint, Status]) {
            pub count HttpAuthLoginCount => "login_count", "Login attempts";
        }
    }
}

mod resolver {
    use super::namespaces::Resolver;
    use super::tags::{Endpoint, Status};

    chalk_metrics::define_metrics! {
        group(namespace = Resolver, tags = [Endpoint]) {
            pub histogram ResolverLatency => "latency",
                "Resolver execution latency",
                tags += [
                    Status as resolver_status,
                    optional Endpoint as resolver_fqn,
                ];
        }
    }
}

fn main() {
    use http::*;
    use resolver::*;
    use tags::*;

    assert_eq!(HttpRequestCount::namespace(), &["http"]);
    assert_eq!(HttpAuthLoginCount::namespace(), &["http", "auth"]);
    assert_eq!(ResolverLatency::namespace(), &["resolver"]);

    HttpRequestCount {
        endpoint: Endpoint::from("/api"),
        status: Status::Success,
    }
    .record();

    HttpRequestCount {
        endpoint: Endpoint::from("/api"),
        status: Status::Failure,
    }
    .record_value(2);

    HttpRequestLatency {
        endpoint: Endpoint::from("/api"),
        status: Status::Success,
    }
    .record(0.02);

    HttpActiveConnections {
        endpoint: Endpoint::from("/api"),
        region: None,
    }
    .record(4.0);

    ResolverLatency {
        endpoint: Endpoint::from("resolver"),
        resolver_status: Status::Success,
        resolver_fqn: Some(Endpoint::from("resolver.fqn")),
    }
    .record(0.005);
}
