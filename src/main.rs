mod config;
mod db;
mod export;
mod filter;
mod import;
mod library;
mod platform;
mod runtime;
mod secrets;
mod ssh;
mod ui;

fn main() -> anyhow::Result<()> {
    config::migrate_from_tabula();
    runtime::runtime();
    ui::run(
        std::env::args_os()
            .skip(1)
            .map(std::path::PathBuf::from)
            .collect(),
    )
}
