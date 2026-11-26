//! 隔离的本机请求实验代理：真实数据库只读，供应商凭据仅驻留内存。
//! 不接管客户端，不写真实设置，不启动主应用；按运行时输出的端口发送实验请求。

use cc_switch_lib::{AppState, AppType, Database, Provider};
use rusqlite::{Connection, OpenFlags};
use serde_json::{json, Value};
use std::{io::Write, path::PathBuf, sync::Arc, time::Duration};

// 只输出本功能既有脱敏审计，不注册其他业务日志，避免凭据或正文落盘。
struct IdentityAuditLogger;
static IDENTITY_AUDIT_LOGGER: IdentityAuditLogger = IdentityAuditLogger;
impl log::Log for IdentityAuditLogger {
    fn enabled(&self, metadata: &log::Metadata<'_>) -> bool {
        metadata.level() <= log::Level::Info && metadata.target().contains("codex_session_identity")
    }
    fn log(&self, record: &log::Record<'_>) {
        if self.enabled(record.metadata()) {
            let message = record.args().to_string();
            if message.starts_with("[CodexSessionIdentity]") {
                eprintln!("{message}");
            }
        }
    }
    fn flush(&self) {}
}

struct Options {
    db: PathBuf,
    home: PathBuf,
    port: u16,
    duration_seconds: u64,
    rotation: bool,
    archive: bool,
}

fn options() -> Result<Options, Box<dyn std::error::Error>> {
    let mut db = None;
    let mut home = None;
    let mut port = 0;
    let mut duration_seconds = 1800;
    let mut rotation = false;
    let mut archive = false;
    let mut args = std::env::args().skip(1);
    while let Some(arg) = args.next() {
        match arg.as_str() {
            "--db" => db = Some(PathBuf::from(args.next().ok_or("--db 缺少路径")?)),
            "--home" => home = Some(PathBuf::from(args.next().ok_or("--home 缺少路径")?)),
            "--port" => port = args.next().ok_or("--port 缺少端口")?.parse()?,
            "--duration-seconds" => {
                duration_seconds = args.next().ok_or("缺少运行时长")?.parse()?
            }
            "--rotation" => rotation = true,
            "--archive" => {
                archive = true;
                rotation = true;
            }
            "--help" | "-h" => {
                println!("隔离本机实验代理：--db <只读实际数据库> [--port 0] [--home <实验目录>] [--duration-seconds 1800] [--rotation] [--archive]");
                std::process::exit(0);
            }
            _ => return Err(format!("未知参数：{arg}").into()),
        }
    }
    Ok(Options {
        db: db.ok_or("必须提供 --db")?,
        home: home.unwrap_or_else(|| {
            std::env::temp_dir().join(format!("cc-switch-probe-{}", std::process::id()))
        }),
        port,
        duration_seconds,
        rotation,
        archive,
    })
}

fn load_current_provider(
    path: &std::path::Path,
    rotation: bool,
    archive: bool,
) -> Result<Provider, Box<dyn std::error::Error>> {
    let conn = Connection::open_with_flags(path, OpenFlags::SQLITE_OPEN_READ_ONLY)?;
    conn.execute_batch("PRAGMA query_only=ON;")?;
    let (id, name, settings, meta, category): (
        String,
        String,
        String,
        Option<String>,
        Option<String>,
    ) = conn.query_row(
        "SELECT id,name,settings_config,meta,category FROM providers WHERE app_type='codex' AND is_current=1 LIMIT 1",
        [],
        |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?, row.get(3)?, row.get(4)?)),
    )?;
    let mut provider = Provider::with_id(id, name, serde_json::from_str(&settings)?, None);
    provider.category = category;
    provider.meta = meta.as_deref().map(serde_json::from_str).transpose()?;
    if let Some(meta) = provider.meta.as_mut() {
        // 固定头覆盖和旧实现的自动重试会混入额外变量，仅在内存副本移除。
        if let Some(overrides) = meta.local_proxy_request_overrides.as_mut() {
            overrides.headers.clear();
        }
        meta.codex_session_identity_rotation = Some(rotation);
        meta.codex_session_history_archive = Some(archive);
        meta.codex_session_identity_diagnostics = Some(rotation);
    }
    Ok(provider)
}

fn main() -> Result<(), Box<dyn std::error::Error>> {
    let options = options()?;
    std::fs::create_dir_all(&options.home)?;
    std::env::set_var("CC_SWITCH_TEST_HOME", &options.home);
    log::set_logger(&IDENTITY_AUDIT_LOGGER)
        .map_err(|_| std::io::Error::other("无法注册脱敏审计日志"))?;
    log::set_max_level(log::LevelFilter::Info);
    let provider = load_current_provider(&options.db, options.rotation, options.archive)?;
    let runtime = tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()?;
    runtime.block_on(async move {
        let db = Arc::new(Database::memory()?);
        db.save_provider(&AppType::Codex, &provider)?;
        db.set_current_provider(&AppType::Codex, &provider.id)?;
        let mut global = db.get_global_proxy_config().await?;
        global.proxy_enabled = true;
        global.listen_address = "127.0.0.1".to_owned();
        global.listen_port = options.port;
        global.enable_logging = false;
        db.update_global_proxy_config(global).await?;
        let mut codex = db.get_proxy_config_for_app(&AppType::Codex).await?;
        codex.enabled = true;
        codex.auto_failover_enabled = false;
        codex.max_retries = 0;
        codex.streaming_first_byte_timeout = 120;
        codex.streaming_idle_timeout = 120;
        codex.non_streaming_timeout = 180;
        db.update_proxy_config_for_app(codex).await?;
        // 开关配置仅写内存数据库；请求内容或凭据均不输出。
        db.set_setting("fork_request_diagnostics_capture", "false")?;
        let state = AppState::new(db);
        let info = state
            .proxy_service
            .start()
            .await
            .map_err(std::io::Error::other)?;
        let ready: Value = json!({
            "状态":"已启动隔离实验代理",
            "进程":std::process::id(),
            "地址":info.address,
            "端口":info.port,
            "真实数据库只读":true,
            "配置及请求日志仅内存":true,
            "固定请求覆盖":false,
            "会话身份自动轮换":options.rotation,
            "完整历史归档":options.archive,
            "客户端接管":false,
            "运行秒数":options.duration_seconds
        });
        println!("{ready}");
        std::io::stdout().flush()?;
        tokio::time::sleep(Duration::from_secs(options.duration_seconds)).await;
        state
            .proxy_service
            .stop()
            .await
            .map_err(std::io::Error::other)?;
        Ok::<(), Box<dyn std::error::Error>>(())
    })
}
