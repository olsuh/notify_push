mod nc;

use crate::config::nc::parse_config_file;
use crate::error::ConfigError;
use crate::{Error, Result};
use derivative::Derivative;
use redis::ConnectionInfo;
use sqlx::any::AnyConnectOptions;
use std::convert::{TryFrom, TryInto};
use std::env::var;
use std::fmt::{Display, Formatter};
use std::net::{IpAddr, Ipv4Addr, SocketAddr};
use std::path::{Path, PathBuf};
use std::str::FromStr;
use structopt::{clap::AppSettings, StructOpt};

#[derive(StructOpt, Debug)]
#[structopt(global_setting = AppSettings::ColoredHelp)]
#[structopt(name = "notify_push")]
pub struct Opt {
    /// The database connect url
    #[structopt(long)]
    pub database_url: Option<AnyConnectOptions>,
    /// The redis connect url
    #[structopt(long)]
    pub redis_url: Vec<ConnectionInfo>,
    /// The table prefix for Nextcloud's database tables
    #[structopt(long)]
    pub database_prefix: Option<String>,
    /// The url the push server can access the nextcloud instance on
    #[structopt(long)]
    pub nextcloud_url: Option<String>,
    /// The port to serve the push server on
    #[structopt(short, long)]
    pub port: Option<u16>,
    /// The port to serve metrics on
    #[structopt(short = "m", long)]
    pub metrics_port: Option<u16>,
    /// The ip address to bind to
    #[structopt(long)]
    pub bind: Option<IpAddr>,
    /// Listen to a unix socket instead of TCP
    #[structopt(long)]
    pub socket_path: Option<PathBuf>,
    /// File permissions for
    #[structopt(long)]
    pub socket_permissions: Option<String>,
    /// Listen to a unix socket instead of TCP for serving metrics
    #[structopt(long)]
    pub metrics_socket_path: Option<PathBuf>,
    /// Disable validating of certificates when connecting to the nextcloud instance
    #[structopt(long)]
    pub allow_self_signed: bool,
    /// The path to the nextcloud config file
    #[structopt(name = "CONFIG_FILE", parse(from_os_str))]
    pub config_file: Option<PathBuf>,
    /// Print the binary version and exit
    #[structopt(long)]
    pub version: bool,
    /// The log level
    #[structopt(long)]
    pub log_level: Option<String>,
    /// Print the parsed config and exit
    #[structopt(long)]
    pub dump_config: bool,
    /// Disable ansi escape sequences in logging output
    #[structopt(long)]
    pub no_ansi: bool,
    /// Load other files named *.config.php in the config folder
    #[structopt(long)]
    pub glob_config: bool,
    /// TLS certificate
    #[structopt(long)]
    pub tls_cert: Option<PathBuf>,
    /// TLS key
    #[structopt(long)]
    pub tls_key: Option<PathBuf>,
    /// The maximum debounce time between messages, in seconds.
    #[structopt(long)]
    pub max_debounce_time: Option<usize>,
    /// The maximum connection time, in seconds. Zero means unlimited.
    #[structopt(long)]
    pub max_connection_time: Option<usize>,
}

#[derive(Debug)]
pub struct Config {
    pub database: AnyConnectOptions,
    pub database_prefix: String,
    pub redis: Vec<ConnectionInfo>,
    pub nextcloud_url: String,
    pub metrics_bind: Option<Bind>,
    pub log_level: String,
    pub bind: Bind,
    pub allow_self_signed: bool,
    pub no_ansi: bool,
    pub tls: Option<TlsConfig>,
    pub max_debounce_time: usize,
    pub max_connection_time: usize,
}

#[derive(Debug, Clone)]
pub struct TlsConfig {
    pub key: PathBuf,
    pub cert: PathBuf,
}

#[derive(Clone, Derivative)]
#[derivative(Debug)]
pub enum Bind {
    Tcp(SocketAddr),
    Unix(
        PathBuf,
        #[derivative(Debug(format_with = "format_permissions"))] u32,
    ),
}

fn format_permissions(permissions: &u32, f: &mut Formatter<'_>) -> std::fmt::Result {
    write!(f, "0{:o}", permissions)
}

impl Display for Bind {
    fn fmt(&self, f: &mut Formatter<'_>) -> std::fmt::Result {
        match self {
            Bind::Tcp(addr) => addr.fmt(f),
            Bind::Unix(path, _) => path.to_string_lossy().fmt(f),
        }
    }
}

impl TryFrom<PartialConfig> for Config {
    type Error = Error;

    fn try_from(config: PartialConfig) -> Result<Self> {
        let socket_permissions = config
            .socket_permissions
            .map(|perm| {
                if perm.len() != 4 && !perm.starts_with('0') {
                    return Err(ConfigError::SocketPermissions(perm, None));
                }
                u32::from_str_radix(&perm, 8)
                    .map_err(|e| ConfigError::SocketPermissions(perm, Some(e)))
            })
            .transpose()?
            .unwrap_or(0o666);
        let bind = match config.socket {
            Some(socket) => Bind::Unix(socket, socket_permissions),
            None => {
                let ip = config
                    .bind
                    .unwrap_or_else(|| IpAddr::V4(Ipv4Addr::new(0, 0, 0, 0)));
                let port = config.port.unwrap_or(7867);
                Bind::Tcp((ip, port).into())
            }
        };

        let metrics_bind = match (config.metrics_socket, config.metrics_port) {
            (Some(socket), _) => Some(Bind::Unix(socket, socket_permissions)),
            (None, Some(port)) => {
                let ip = config
                    .bind
                    .unwrap_or_else(|| IpAddr::V4(Ipv4Addr::new(0, 0, 0, 0)));
                Some(Bind::Tcp((ip, port).into()))
            }
            _ => None,
        };

        let mut nextcloud_url = config
            .nextcloud_url
            .ok_or_else(|| ConfigError::NoNextcloud)?;
        if !nextcloud_url.ends_with('/') {
            nextcloud_url.push('/');
        }

        Ok(Config {
            database: config.database.ok_or_else(|| ConfigError::NoDatabase)?,
            database_prefix: config
                .database_prefix
                .unwrap_or_else(|| String::from("oc_")),
            redis: config.redis,
            nextcloud_url,
            metrics_bind,
            log_level: config.log_level.unwrap_or_else(|| String::from("warn")),
            bind,
            allow_self_signed: config.allow_self_signed.unwrap_or(false),
            no_ansi: config.no_ansi.unwrap_or(false),
            tls: config.tls,
            max_debounce_time: config.max_debounce_time.unwrap_or(15),
            max_connection_time: config.max_connection_time.unwrap_or(0),
        })
    }
}

impl Config {
    pub fn from_opt(opt: Opt) -> Result<Self> {
        let from_config = opt
            .config_file
            .as_ref()
            .map(|path| PartialConfig::from_file(path, opt.glob_config))
            .transpose()?
            .unwrap_or_default();
        let from_env = PartialConfig::from_env()?;
        let from_opt = PartialConfig::from_opt(opt);

        from_opt.merge(from_env).merge(from_config).try_into()
    }
}

#[derive(Debug, Default)]
struct PartialConfig {
    pub database: Option<AnyConnectOptions>,
    pub database_prefix: Option<String>,
    pub redis: Vec<ConnectionInfo>,
    pub nextcloud_url: Option<String>,
    pub port: Option<u16>,
    pub metrics_port: Option<u16>,
    pub metrics_socket: Option<PathBuf>,
    pub log_level: Option<String>,
    pub bind: Option<IpAddr>,
    pub socket: Option<PathBuf>,
    pub socket_permissions: Option<String>,
    pub allow_self_signed: Option<bool>,
    pub no_ansi: Option<bool>,
    pub tls: Option<TlsConfig>,
    pub max_debounce_time: Option<usize>,
    pub max_connection_time: Option<usize>,
}

impl PartialConfig {
    fn from_env() -> Result<Self> {
        let database = parse_var("DATABASE_URL")?;
        let database_prefix = var("DATABASE_PREFIX").ok();
        let redis = parse_var("REDIS_URL")?;
        let nextcloud_url = var("NEXTCLOUD_URL").ok();
        let port = parse_var("PORT")?;
        let metrics_port = parse_var("METRICS_PORT")?;
        let metrics_socket = parse_var("METRICS_SOCKET_PATH")?;
        let log_level = var("LOG").ok();
        let bind = parse_var("BIND")?;
        let socket = var("SOCKET_PATH").map(PathBuf::from).ok();
        let socket_permissions = var("SOCKET_PERMISSIONS").ok();
        let allow_self_signed = var("ALLOW_SELF_SIGNED").map(|val| val == "true").ok();
        let no_ansi = var("NO_ANSI").map(|val| val == "true").ok();

        let tls_cert = parse_var("TLS_CERT")?;
        let tls_key = parse_var("TLS_KEY")?;

        let tls = if let (Some(cert), Some(key)) = (tls_cert, tls_key) {
            Some(TlsConfig { cert, key })
        } else {
            None
        };
        let max_debounce_time = parse_var("MAX_DEBOUNCE_TIME")?;
        let max_connection_time = parse_var("MAX_CONNECTION_TIME")?;

        Ok(PartialConfig {
            database,
            database_prefix,
            redis: redis.into_iter().collect(),
            nextcloud_url,
            port,
            metrics_port,
            metrics_socket,
            log_level,
            bind,
            socket,
            socket_permissions,
            allow_self_signed,
            no_ansi,
            tls,
            max_debounce_time,
            max_connection_time,
        })
    }

    fn from_file(file: impl AsRef<Path>, glob: bool) -> Result<Self> {
        Ok(parse_config_file(file, glob)?)
    }

    fn from_opt(opt: Opt) -> Self {
        let tls = if let (Some(cert), Some(key)) = (opt.tls_cert, opt.tls_key) {
            Some(TlsConfig { cert, key })
        } else {
            None
        };

        PartialConfig {
            database: opt.database_url,
            database_prefix: opt.database_prefix,
            redis: opt.redis_url,
            nextcloud_url: opt.nextcloud_url,
            port: opt.port,
            metrics_port: opt.metrics_port,
            metrics_socket: opt.metrics_socket_path,
            log_level: opt.log_level,
            bind: opt.bind,
            socket: opt.socket_path,
            socket_permissions: opt.socket_permissions,
            allow_self_signed: if opt.allow_self_signed {
                Some(true)
            } else {
                None
            },
            no_ansi: if opt.no_ansi { Some(true) } else { None },
            tls,
            max_debounce_time: opt.max_debounce_time,
            max_connection_time: opt.max_connection_time,
        }
    }

    fn merge(self, fallback: Self) -> Self {
        PartialConfig {
            database: self.database.or(fallback.database),
            database_prefix: self.database_prefix.or(fallback.database_prefix),
            redis: if self.redis.is_empty() {
                fallback.redis
            } else {
                self.redis
            },
            nextcloud_url: self.nextcloud_url.or(fallback.nextcloud_url),
            port: self.port.or(fallback.port),
            metrics_port: self.metrics_port.or(fallback.metrics_port),
            metrics_socket: self.metrics_socket.or(fallback.metrics_socket),
            log_level: self.log_level.or(fallback.log_level),
            bind: self.bind.or(fallback.bind),
            socket: self.socket.or(fallback.socket),
            socket_permissions: self.socket_permissions.or(fallback.socket_permissions),
            allow_self_signed: self.allow_self_signed.or(fallback.allow_self_signed),
            no_ansi: self.no_ansi.or(fallback.no_ansi),
            tls: self.tls.or(fallback.tls),
            max_debounce_time: self.max_debounce_time.or(fallback.max_debounce_time),
            max_connection_time: self.max_connection_time.or(fallback.max_connection_time),
        }
    }
}

fn parse_var<T>(name: &'static str) -> Result<Option<T>>
where
    T: FromStr + 'static,
    T::Err: std::error::Error + Sync + Send,
{
    var(name)
        .ok()
        .map(|val| T::from_str(&val))
        .transpose()
        .map_err(|e| ConfigError::Env(name, Box::new(e)).into())
}
