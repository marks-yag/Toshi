use std::fs::create_dir;
use std::net::{IpAddr, SocketAddr};
use std::path::{Path, PathBuf};
use std::sync::{Arc, RwLock};

use futures::{future, Future};
use log::{error, info};
use tokio::runtime::Runtime;
use tokio::sync::oneshot;

use std::error::Error;
use toshi::cluster::rpc_server::RpcServer;
use toshi::commit::IndexWatcher;
use toshi::index::IndexCatalog;
use toshi::router::router_with_catalog;
use toshi::settings::{Settings, HEADER, RPC_HEADER};
use toshi::{cluster, shutdown, support};

pub fn main() -> Result<(), ()> {
    let settings = support::settings();

    std::env::set_var("RUST_LOG", &settings.log_level);
    pretty_env_logger::init();
    info!("{:?}", &settings);

    let mut rt = Runtime::new().expect("failed to start new Runtime");

    let (tx, shutdown_signal) = oneshot::channel();

    if !Path::new(&settings.path).exists() {
        info!("Base data path {} does not exist, creating it...", settings.path);
        create_dir(settings.path.clone()).expect("Unable to create data directory");
    }

    let index_catalog = {
        let path = PathBuf::from(settings.path.clone());
        let index_catalog = match IndexCatalog::new(path, settings.clone()) {
            Ok(v) => v,
            Err(e) => {
                eprintln!("Error creating IndexCatalog from path {} - {}", settings.path, e);
                std::process::exit(1);
            }
        };

        Arc::new(RwLock::new(index_catalog))
    };

    let toshi = {
        let server = if settings.experimental && settings.experimental_features.master {
            future::Either::A(run_master(Arc::clone(&index_catalog), &settings))
        } else {
            future::Either::B(run_data(Arc::clone(&index_catalog), &settings))
        };
        let shutdown = shutdown::shutdown(tx);
        server.select(shutdown)
    };

    rt.spawn(toshi.map(|_| ()).map_err(|_| ()));

    shutdown_signal
        .map_err(|e| unreachable!("Shutdown signal channel should not error, This is a bug. \n {:?} ", e.description()))
        .and_then(move |_| {
            index_catalog
                .write()
                .expect("Unable to acquire write lock on index catalog")
                .clear();
            Ok(())
        })
        .and_then(move |_| rt.shutdown_now())
        .wait()
}

fn create_watcher(catalog: Arc<RwLock<IndexCatalog>>, settings: &Settings) -> impl Future<Item = (), Error = ()> {
    if settings.auto_commit_duration > 0 {
        let commit_watcher = IndexWatcher::new(catalog.clone(), settings.auto_commit_duration);
        future::Either::A(future::lazy(move || {
            commit_watcher.start();
            future::ok::<(), ()>(())
        }))
    } else {
        future::Either::B(future::ok::<(), ()>(()))
    }
}

fn run_data(catalog: Arc<RwLock<IndexCatalog>>, settings: &Settings) -> impl Future<Item = (), Error = ()> {
    let commit_watcher = create_watcher(Arc::clone(&catalog), settings);
    let addr: IpAddr = settings.host.parse().expect(&format!("Invalid ip address: {}", &settings.host));
    let settings = settings.clone();
    let bind: SocketAddr = SocketAddr::new(addr, settings.port);

    println!("{}", RPC_HEADER);
    info!("I am a data node...Binding to: {}", addr);
    commit_watcher.and_then(move |_| RpcServer::serve(bind, catalog))
}

fn run_master(catalog: Arc<RwLock<IndexCatalog>>, settings: &Settings) -> impl Future<Item = (), Error = ()> {
    let commit_watcher = create_watcher(Arc::clone(&catalog), settings);
    let addr: IpAddr = settings.host.parse().expect(&format!("Invalid ip address: {}", &settings.host));
    let bind: SocketAddr = SocketAddr::new(addr, settings.port);

    println!("{}", HEADER);

    if settings.experimental {
        let settings = settings.clone();
        let place_addr = settings.place_addr.clone();
        let consul_addr = settings.experimental_features.consul_addr.clone();
        let cluster_name = settings.experimental_features.cluster_name.clone();
        let nodes = settings.experimental_features.nodes.clone();

        let run = commit_watcher.and_then(move |_| {
            if nodes.is_empty() {
                tokio::spawn(cluster::connect_to_consul(&settings));
                let consul = cluster::Consul::builder()
                    .with_cluster_name(cluster_name)
                    .with_address(consul_addr)
                    .build()
                    .expect("Could not build Consul client.");

                let place_addr = place_addr.parse().expect("Placement address must be a valid SocketAddr");
                tokio::spawn(cluster::run(place_addr, consul).map_err(|e| error!("Error with running cluster: {}", e)));
            } else {
                let update = catalog.read().unwrap().update_remote_indexes();
                tokio::spawn(update);
            }
            router_with_catalog(&bind, &catalog)
        });
        future::Either::A(run)
    } else {
        let run = commit_watcher.and_then(move |_| router_with_catalog(&bind, &catalog));
        future::Either::B(run)
    }
}
