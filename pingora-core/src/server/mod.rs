// Copyright 2024 Cloudflare, Inc.
//
// Licensed under the Apache License, Version 2.0 (the "License");
// you may not use this file except in compliance with the License.
// You may obtain a copy of the License at
//
// http://www.apache.org/licenses/LICENSE-2.0
//
// Unless required by applicable law or agreed to in writing, software
// distributed under the License is distributed on an "AS IS" BASIS,
// WITHOUT WARRANTIES OR CONDITIONS OF ANY KIND, either express or implied.
// See the License for the specific language governing permissions and
// limitations under the License.

//! Server process and configuration management

use std::sync::Arc;
use std::thread;

use log::{debug, error, info};
use tokio::signal::unix;
use tokio::sync::{Mutex, watch};
use tokio::time::{Duration, sleep};

use configuration::{Opt, ServerConf};
use daemon::daemonize;
use pingora_error::{Error, ErrorType, Result};
use pingora_runtime::Runtime;
use pingora_timeout::fast_timeout;
use transfer_fd::Fds;

use crate::services::Service;

pub mod configuration;
mod daemon;
pub(crate) mod transfer_fd;

/* time to wait before exiting the program
this is the graceful period for all existing session to finish */
const EXIT_TIMEOUT: u64 = 60 * 5;
/* time to wait before shutting down listening sockets
this is the graceful period for the new service to get ready */
const CLOSE_TIMEOUT: u64 = 5;

enum ShutdownType {
    Graceful,
    Quick,
}

/// The receiver for server's shutdown event. The value will turn to true once the server starts
/// to shutdown
pub type ShutdownWatch = watch::Receiver<bool>;
pub(crate) type ListenFds = Arc<Mutex<Fds>>;

/// The server object
///
/// This object represents an entire pingora server process which may have multiple independent
/// services (see [crate::services]). The server object handles signals, reading configuration,
/// zero downtime upgrade and error reporting.
pub struct Server {
    services: Vec<Box<dyn Service>>,
    listen_fds: Option<ListenFds>,
    shutdown_watch: watch::Sender<bool>,
    // TODO: we many want to drop this copy to let sender call closed()
    shutdown_recv: ShutdownWatch,
    /// the parsed server configuration
    pub configuration: Arc<ServerConf>,
    /// the parser command line options
    pub options: Option<Opt>,
    /// the Sentry DSN
    ///
    /// Panics and other events sentry captures will send to this DSN **only in release mode**
    pub sentry: Option<String>,
}

// TODO: delete the pid when exit

impl Server {
    async fn main_loop(&self) -> ShutdownType {
        // waiting for exit signal
        let shutdown_signal = wait_for_shutdown_signal().await;
        match shutdown_signal {
            ShutdownSignal::Fast => {
                info!("SIGINT received, exiting");
                ShutdownType::Quick
            }
            ShutdownSignal::GracefulTerminate => {
                // we receive a graceful terminate, all instances are instructed to stop
                info!("SIGTERM received, gracefully exiting");
                // graceful shutdown if there are listening sockets
                info!("Broadcasting graceful shutdown");
                match self.shutdown_watch.send(true) {
                    Ok(_) => {
                        info!("Graceful shutdown started!");
                    }
                    Err(e) => {
                        error!("Graceful shutdown broadcast failed: {e}");
                    }
                }
                info!("Broadcast graceful shutdown complete");
                ShutdownType::Graceful
            }
            ShutdownSignal::GracefulUpgrade => {
                let mut wait_for_sig_int = unix::signal(unix::SignalKind::interrupt())
                    .expect("Failed to create SIGINT listener.");
                tokio::select! {
                    _ = wait_for_sig_int.recv() => {}
                    _ = self.graceful_upgrade() => {}
                }
                ShutdownType::Graceful
            }
        }
    }

    async fn graceful_upgrade(&self) {
        // aka: move below to another task and only kick it off here
        info!("SIGQUIT received, sending socks and gracefully exiting");
        if let Some(result) = self.send_fds().await {
            info!("Trying to send socks");
            // XXX: this is blocking IO
            match result {
                Ok(_) => {
                    info!("listener sockets sent");
                }
                Err(e) => {
                    error!("Unable to send listener sockets to new process: {e}");
                    // sentry log error on fd send failure
                    #[cfg(not(debug_assertions))]
                    sentry::capture_error(&e);
                }
            }
            sleep(Duration::from_secs(CLOSE_TIMEOUT)).await;
            info!("Broadcasting graceful shutdown");
            // gracefully exiting
            match self.shutdown_watch.send(true) {
                Ok(_) => {
                    info!("Graceful shutdown started!");
                }
                Err(e) => {
                    error!("Graceful shutdown broadcast failed: {e}");
                    // switch to fast shutdown
                }
            }
            info!("Broadcast graceful shutdown complete");
        } else {
            info!("No socks to send, shutting down.");
        }
    }

    fn run_service(
        mut service: Box<dyn Service>,
        fds: Option<ListenFds>,
        shutdown: ShutdownWatch,
        threads: usize,
        work_stealing: bool,
    ) -> Runtime
// NOTE: we need to keep the runtime outside async since
    // otherwise the runtime will be dropped.
    {
        let service_runtime = Server::create_runtime(service.name(), threads, work_stealing);
        service_runtime.get_handle().spawn(async move {
            service.start_service(fds, shutdown).await;
            info!("service exited.")
        });
        service_runtime
    }

    /// Send all listening sockets to new server.
    ///
    /// When trying to zero downtime upgrade as a new server from older which is already
    /// running, this function will try to send all its listening sockets to the new one.
    pub async fn send_fds(&self) -> Option<Result<usize, nix::Error>> {
        if let Some(fds) = &self.listen_fds {
            let fds = fds.lock().await;
            info!("Trying to send socks");
            return Some(fds.send_to_sock(self.configuration.as_ref().upgrade_sock.as_str()));
        }
        None
    }

    fn load_fds(&mut self, upgrade: bool) -> Result<(), nix::Error> {
        let mut fds = Fds::new();
        if upgrade {
            debug!("Trying to receive socks");
            fds.get_from_sock(self.configuration.as_ref().upgrade_sock.as_str())?
        }
        self.listen_fds = Some(Arc::new(Mutex::new(fds)));
        Ok(())
    }

    /// Create a new [`Server`].
    ///
    /// Only one [`Server`] needs to be created for a process. A [`Server`] can hold multiple
    /// independent services.
    ///
    /// Command line options can either be passed by parsing the command line arguments via
    /// `Opt::from_args()`, or be generated by other means.
    pub fn new(opt: impl Into<Option<Opt>>) -> Result<Server> {
        let opt = opt.into();
        let (tx, rx) = watch::channel(false);

        let conf = if let Some(opt) = opt.as_ref() {
            opt.conf.as_ref().map_or_else(
                || {
                    // options, no conf, generated
                    ServerConf::new_with_opt_override(opt).ok_or_else(|| {
                        Error::explain(ErrorType::ReadError, "Conf generation failed")
                    })
                },
                |_| {
                    // options and conf loaded
                    ServerConf::load_yaml_with_opt_override(opt)
                },
            )
        } else {
            ServerConf::new()
                .ok_or_else(|| Error::explain(ErrorType::ReadError, "Conf generation failed"))
        }?;

        Ok(Server {
            services: vec![],
            listen_fds: None,
            shutdown_watch: tx,
            shutdown_recv: rx,
            configuration: Arc::new(conf),
            options: opt,
            sentry: None,
        })
    }

    /// Add a service to this server.
    ///
    /// A service is anything that implements [`Service`].
    pub fn add_service(&mut self, service: impl Service + 'static) {
        self.services.push(Box::new(service));
    }

    /// Similar to [`Self::add_service()`], but take a list of services
    pub fn add_services(&mut self, services: Vec<Box<dyn Service>>) {
        self.services.extend(services);
    }

    /// Prepare the server to start
    ///
    /// When trying to zero downtime upgrade from an older version of the server which is already
    /// running, this function will try to get all its listening sockets in order to take them over.
    pub fn bootstrap(&mut self) {
        info!("Bootstrap starting");
        debug!("{:#?}", self.options);

        /* only init sentry in release builds */
        #[cfg(not(debug_assertions))]
            let _guard = match self.sentry.as_ref() {
            Some(uri) => Some(sentry::init(uri.as_str())),
            None => None,
        };

        if self.options.as_ref().map_or(false, |o| o.test) {
            info!("Server Test passed, exiting");
            std::process::exit(0);
        }

        // load fds
        match self.load_fds(self.options.as_ref().map_or(false, |o| o.upgrade)) {
            Ok(_) => {
                info!("Bootstrap done");
            }
            Err(e) => {
                // sentry log error on fd load failure
                #[cfg(not(debug_assertions))]
                sentry::capture_error(&e);

                error!("Bootstrap failed on error: {:?}, exiting.", e);
                std::process::exit(1);
            }
        }
    }

    /// Run all services of server
    ///
    /// This function will run all services of server.
    pub fn run_services(&mut self) -> Vec<Runtime> {
        let conf = self.configuration.as_ref();
        let mut runtimes: Vec<Runtime> = Vec::new();

        while let Some(service) = self.services.pop() {
            let threads = service.threads().unwrap_or(conf.threads);
            let runtime = Server::run_service(
                service,
                self.listen_fds.clone(),
                self.shutdown_recv.clone(),
                threads,
                conf.work_stealing,
            );
            runtimes.push(runtime);
        }
        runtimes
    }

    /// Start the server
    ///
    /// This function will block forever until the server needs to quit. So this would be the last
    /// function to call for this object.
    ///
    /// Note: this function may fork the process for daemonization, so any additional threads created
    /// before this function will be lost to any service logic once this function is called.
    pub fn run_forever(mut self) -> ! {
        info!("Server starting");

        let conf = self.configuration.as_ref();

        if conf.daemon {
            info!("Daemonizing the server");
            fast_timeout::pause_for_fork();
            daemonize(&self.configuration);
            fast_timeout::unpause();
        }

        /* only init sentry in release builds */
        #[cfg(not(debug_assertions))]
            let _guard = match self.sentry.as_ref() {
            Some(uri) => Some(sentry::init(uri.as_str())),
            None => None,
        };

        let runtimes = self.run_services();

        // blocked on main loop so that it runs forever
        // Only work steal runtime can use block_on()
        let server_runtime = Server::create_runtime("Server", 1, true);
        let shutdown_type = server_runtime.get_handle().block_on(self.main_loop());

        if matches!(shutdown_type, ShutdownType::Graceful) {
            info!("Graceful shutdown: grace period {}s starts", EXIT_TIMEOUT);
            thread::sleep(Duration::from_secs(EXIT_TIMEOUT));
            info!("Graceful shutdown: grace period ends");
        }

        // Give tokio runtimes time to exit
        let shutdown_timeout = match shutdown_type {
            ShutdownType::Quick => Duration::from_secs(0),
            ShutdownType::Graceful => Duration::from_secs(5),
        };
        let shutdowns: Vec<_> = runtimes
            .into_iter()
            .map(|rt| {
                info!("Waiting for runtimes to exit!");
                thread::spawn(move || {
                    rt.shutdown_timeout(shutdown_timeout);
                    thread::sleep(shutdown_timeout)
                })
            })
            .collect();
        for shutdown in shutdowns {
            if let Err(e) = shutdown.join() {
                error!("Failed to shutdown runtime: {:?}", e);
            }
        }
        info!("All runtimes exited, exiting now");
        std::process::exit(0)
    }

    fn create_runtime(name: &str, threads: usize, work_steal: bool) -> Runtime {
        if work_steal {
            Runtime::new_steal(threads, name)
        } else {
            Runtime::new_no_steal(threads, name)
        }
    }
}

enum ShutdownSignal {
    Fast,
    GracefulTerminate,
    GracefulUpgrade,
}

async fn wait_for_shutdown_signal() -> ShutdownSignal {
    let sig_int = async {
        tokio::signal::ctrl_c()
            .await
            .expect("Failed to create SIGINT listener.");
    };

    #[cfg(unix)]
        let sig_term = async {
        unix::signal(unix::SignalKind::terminate())
            .expect("Failed to create SIGTERM listener.")
            .recv()
            .await;
    };

    #[cfg(unix)]
        let sig_quit = async {
        unix::signal(unix::SignalKind::quit())
            .expect("Failed to create SIGQUIT listener.")
            .recv()
            .await;
    };

    #[cfg(not(unix))]
        let sig_term = std::future::pending::<()>();

    #[cfg(not(unix))]
        let sig_quit = std::future::pending::<()>();

    tokio::select! {
        _ = sig_int => ShutdownSignal::Fast,
        _ = sig_term => ShutdownSignal::GracefulTerminate,
        _ = sig_quit => ShutdownSignal::GracefulUpgrade,
    }
}