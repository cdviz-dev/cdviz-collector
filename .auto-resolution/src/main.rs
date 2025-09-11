use miette::Result;

#[tokio::main]
async fn main() -> Result<()> {
    cdviz_collector::run_with_sys_args().await
}
