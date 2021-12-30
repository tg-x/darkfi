use std::{path::PathBuf, sync::Arc};

use async_executor::Executor;
use clap::clap_app;
use easy_parallel::Parallel;
use log::debug;
use simplelog::{LevelFilter, SimpleLogger};

use drk::{
    blockchain::{rocks::columns, Rocks, RocksColumn},
    cli::{Config, GatewaydConfig},
    service::GatewayService,
    util::{expand_path, join_config_path},
    Result,
};

async fn start(executor: Arc<Executor<'_>>, config: &GatewaydConfig) -> Result<()> {
    let rocks = Rocks::new(&expand_path(&config.database_path)?)?;
    let rocks_slabstore_column = RocksColumn::<columns::Slabs>::new(rocks);

    let gateway = GatewayService::new(
        config.protocol_listen_address,
        config.publisher_listen_address,
        rocks_slabstore_column,
    )?;

    Ok(gateway.start(executor.clone()).await?)
}

#[async_std::main]
async fn main() -> Result<()> {
    let args = clap_app!(gatewayd =>
        (@arg CONFIG: -c --config +takes_value "Sets a custom config file")
        (@arg verbose: -v --verbose "Increase verbosity")
        (@arg trace: -t --trace "Show event trace")
    )
    .get_matches();

    let config_path = if args.is_present("CONFIG") {
        expand_path(args.value_of("CONFIG").unwrap())?
    } else {
        join_config_path(&PathBuf::from("gatewayd.toml"))?
    };

    let loglevel = if args.is_present("verbose") {
        LevelFilter::Debug
    } else if args.is_present("trace") {
        LevelFilter::Trace
    } else {
        LevelFilter::Info
    };

    SimpleLogger::init(loglevel, simplelog::Config::default())?;

    let config: GatewaydConfig = Config::<GatewaydConfig>::load(config_path)?;

    let ex = Arc::new(Executor::new());
    let (signal, shutdown) = async_channel::unbounded::<()>();

    let ex2 = ex.clone();

    let nthreads = num_cpus::get();
    debug!(target: "GATEWAY DAEMON", "Run {} executor threads", nthreads);

    let (_, result) = Parallel::new()
        .each(0..nthreads, |_| smol::future::block_on(ex.run(shutdown.recv())))
        // Run the main future on the current thread.
        .finish(|| {
            smol::future::block_on(async move {
                start(ex2, &config).await?;
                drop(signal);
                Ok::<(), drk::Error>(())
            })
        });

    result
}