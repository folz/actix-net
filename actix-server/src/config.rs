use std::collections::HashMap;
use std::{fmt, io, net};

use actix_server_config::{Io, ServerConfig};
use actix_service::{IntoNewService, NewService};
use futures::future::{join_all, Future};
use log::error;
use tokio_tcp::TcpStream;

use crate::counter::CounterGuard;

use super::builder::bind_addr;
use super::services::{
    BoxedServerService, InternalServiceFactory, ServerMessage, StreamService,
};
use super::Token;

pub struct ServiceConfig {
    pub(crate) services: Vec<(String, net::TcpListener)>,
    pub(crate) apply: Option<Box<ServiceRuntimeConfiguration>>,
    pub(crate) threads: usize,
    pub(crate) backlog: i32,
}

impl ServiceConfig {
    pub(super) fn new(threads: usize, backlog: i32) -> ServiceConfig {
        ServiceConfig {
            threads,
            backlog,
            services: Vec::new(),
            apply: None,
        }
    }

    /// Set number of workers to start.
    ///
    /// By default server uses number of available logical cpu as workers
    /// count.
    pub fn workers(&mut self, num: usize) {
        self.threads = num;
    }

    /// Add new service to server
    pub fn bind<U, N: AsRef<str>>(&mut self, name: N, addr: U) -> io::Result<&mut Self>
    where
        U: net::ToSocketAddrs,
    {
        let sockets = bind_addr(addr, self.backlog)?;

        for lst in sockets {
            self.listen(name.as_ref(), lst);
        }

        Ok(self)
    }

    /// Add new service to server
    pub fn listen<N: AsRef<str>>(&mut self, name: N, lst: net::TcpListener) -> &mut Self {
        if self.apply.is_none() {
            self.apply = Some(Box::new(not_configured));
        }
        self.services.push((name.as_ref().to_string(), lst));
        self
    }

    /// Register service configuration function. This function get called
    /// during worker runtime configuration. It get executed in worker thread.
    pub fn apply<F>(&mut self, f: F) -> io::Result<()>
    where
        F: Fn(&mut ServiceRuntime) + Send + Clone + 'static,
    {
        self.apply = Some(Box::new(f));
        Ok(())
    }
}

pub(super) struct ConfiguredService {
    rt: Box<ServiceRuntimeConfiguration>,
    names: HashMap<Token, (String, net::SocketAddr)>,
    services: HashMap<String, Token>,
}

impl ConfiguredService {
    pub(super) fn new(rt: Box<ServiceRuntimeConfiguration>) -> Self {
        ConfiguredService {
            rt,
            names: HashMap::new(),
            services: HashMap::new(),
        }
    }

    pub(super) fn stream(&mut self, token: Token, name: String, addr: net::SocketAddr) {
        self.names.insert(token, (name.clone(), addr));
        self.services.insert(name, token);
    }
}

impl InternalServiceFactory for ConfiguredService {
    fn name(&self, token: Token) -> &str {
        &self.names[&token].0
    }

    fn clone_factory(&self) -> Box<InternalServiceFactory> {
        Box::new(Self {
            rt: self.rt.clone(),
            names: self.names.clone(),
            services: self.services.clone(),
        })
    }

    fn create(&self) -> Box<Future<Item = Vec<(Token, BoxedServerService)>, Error = ()>> {
        // configure services
        let mut rt = ServiceRuntime::new(self.services.clone());
        self.rt.configure(&mut rt);
        rt.validate();

        // construct services
        let mut fut = Vec::new();
        for (token, ns) in rt.services {
            let config = ServerConfig::new(self.names[&token].1);
            fut.push(ns.new_service(&config).map(move |service| (token, service)));
        }

        // on start futures
        if rt.onstart.is_empty() {
            Box::new(join_all(fut).map_err(|e| {
                error!("Can not construct service: {:?}", e);
            }))
        } else {
            // run onstart future and then construct services
            Box::new(
                join_all(rt.onstart)
                    .map_err(|e| {
                        error!("Can not construct service: {:?}", e);
                    })
                    .and_then(move |_| {
                        join_all(fut).map_err(|e| {
                            error!("Can not construct service: {:?}", e);
                        })
                    }),
            )
        }
    }
}

pub(super) trait ServiceRuntimeConfiguration: Send {
    fn clone(&self) -> Box<ServiceRuntimeConfiguration>;

    fn configure(&self, rt: &mut ServiceRuntime);
}

impl<F> ServiceRuntimeConfiguration for F
where
    F: Fn(&mut ServiceRuntime) + Send + Clone + 'static,
{
    fn clone(&self) -> Box<ServiceRuntimeConfiguration> {
        Box::new(self.clone())
    }

    fn configure(&self, rt: &mut ServiceRuntime) {
        (self)(rt)
    }
}

fn not_configured(_: &mut ServiceRuntime) {
    error!("Service is not configured");
}

pub struct ServiceRuntime {
    names: HashMap<String, Token>,
    services: HashMap<Token, BoxedNewService>,
    onstart: Vec<Box<Future<Item = (), Error = ()>>>,
}

impl ServiceRuntime {
    fn new(names: HashMap<String, Token>) -> Self {
        ServiceRuntime {
            names,
            services: HashMap::new(),
            onstart: Vec::new(),
        }
    }

    fn validate(&self) {
        for (name, token) in &self.names {
            if !self.services.contains_key(&token) {
                error!("Service {:?} is not configured", name);
            }
        }
    }

    /// Register service.
    ///
    /// Name of the service must be registered during configuration stage with
    /// *ServiceConfig::bind()* or *ServiceConfig::listen()* methods.
    pub fn service<T, F>(&mut self, name: &str, service: F)
    where
        F: IntoNewService<T, ServerConfig>,
        T: NewService<ServerConfig, Request = Io<TcpStream>> + 'static,
        T::Future: 'static,
        T::Service: 'static,
        T::InitError: fmt::Debug,
    {
        // let name = name.to_owned();
        if let Some(token) = self.names.get(name) {
            self.services.insert(
                token.clone(),
                Box::new(ServiceFactory {
                    inner: service.into_new_service(),
                }),
            );
        } else {
            panic!("Unknown service: {:?}", name);
        }
    }

    /// Execute future before services initialization.
    pub fn on_start<F>(&mut self, fut: F)
    where
        F: Future<Item = (), Error = ()> + 'static,
    {
        self.onstart.push(Box::new(fut))
    }
}

type BoxedNewService = Box<
    NewService<
        ServerConfig,
        Request = (Option<CounterGuard>, ServerMessage),
        Response = (),
        Error = (),
        InitError = (),
        Service = BoxedServerService,
        Future = Box<Future<Item = BoxedServerService, Error = ()>>,
    >,
>;

struct ServiceFactory<T> {
    inner: T,
}

impl<T> NewService<ServerConfig> for ServiceFactory<T>
where
    T: NewService<ServerConfig, Request = Io<TcpStream>>,
    T::Future: 'static,
    T::Service: 'static,
    T::Error: 'static,
    T::InitError: fmt::Debug + 'static,
{
    type Request = (Option<CounterGuard>, ServerMessage);
    type Response = ();
    type Error = ();
    type InitError = ();
    type Service = BoxedServerService;
    type Future = Box<Future<Item = BoxedServerService, Error = ()>>;

    fn new_service(&self, cfg: &ServerConfig) -> Self::Future {
        Box::new(self.inner.new_service(cfg).map_err(|_| ()).map(|s| {
            let service: BoxedServerService = Box::new(StreamService::new(s));
            service
        }))
    }
}
