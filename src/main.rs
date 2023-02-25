use anyhow::{anyhow, Result};
use clap::Parser;
use fastcgi_client::Request;
use fastcgi_client::{Params, Client};
use std::borrow::Cow;
use std::env;
use std::path::{Path, PathBuf};
use std::process::ExitCode;
use tokio::{
    fs::OpenOptions,
    io,
    net::{TcpStream, UnixStream}
};
use url::{Host, Url};

const CGI_META_VARS: &[&str] = &[
    "AUTH_TYPE",
    "CONTENT_LENGTH",
    "CONTENT_TYPE",
    "GATEWAY_INTERFACE",
    "PATH_INFO",
    "PATH_TRANSLATED",
    "QUERY_STRING",
    "REMOTE_ADDR",
    "REMOTE_HOST",
    "REMOTE_IDENT",
    "REMOTE_USER",
    "REQUEST_METHOD",
    "SCRIPT_NAME",
    "SERVER_NAME",
    "SERVER_PORT",
    "SERVER_PROTOCOL",
    "SERVER_SOFTWARE",
];

#[derive(Parser, Debug)]
#[command(name = "FastCGI CLI")]
#[command(author = "Harry T. Vennik <htvennik@gmail.com>")]
#[command(version = env!("CARGO_PKG_VERSION"))]
#[command(about = "Send request to FastCGI server.")]
#[command(long_about = "CLI tool to interact with a FastCGI server directly. Also deployable as a CGI-to-FastCGI bridge.")]
struct Cli {
    /**
        Address of FastCGI server

        May be either HOST:PORT or a PATH to a unix socket.
    */
    address: String,

    /**
        URL to be accessed

        It is bluntly assumed that this URL is served by the FastCGI server at ADDRESS.
        The scheme, hostname and path are passed on to the FastCGI server as appropriate.
     */
    url: Option<Url>,

    /// Send given string as request body
    #[arg(long = "data", group = "grp_data")]
    data: Option<String>,

    /// Set the document root (PATH should be a valid absolute path at the server, no trailing slash)
    #[arg(long = "root")]
    server_document_root: Option<String>,

    /// Set the SCRIPT_NAME
    #[arg(long = "script")]
    script_name: Option<String>,

    /// Send environment variable VAR as FastCGI parameter
    #[arg(short = 'e', long = "pass-env", value_name = "VAR")]
    env_vars: Vec<String>,

    /// Pass only excplicitly whitelisted environment variables
    #[arg(long = "no-env")]
    env_clear: bool,

    /// Pass all environment variables unmodified (default behavior is to pass only CGI-defined metavariables and protocol variables)
    #[arg(short = 'E', long = "full-env", conflicts_with = "env_clear")]
    env_full: bool,

    /// Write ouput files to DIR
    #[arg(long = "output-dir", value_name = "DIR")]
    output_directory: Option<PathBuf>,

    /// Send output to specified file
    #[arg(short = 'o', long = "output", value_name = "FILE", conflicts_with = "output_file_remote_name")]
    output_file_name: Option<PathBuf>,

    /// Use the final segment of the URL path as output filename
    #[arg(short = 'O', long = "remote-name", requires = "url")]
    output_file_remote_name: bool,

    /// Send output received on the FastCGI STDERR stream to specified file. Error output generated locally will still be written to actual stderr.
    #[arg(long = "stderr", value_name = "FILE")]
    stderr_file_name: Option<PathBuf>,

    /// Set FastCGI parameter REQUEST_METHOD
    #[arg(short = 'X', long = "request", value_name = "METHOD", default_value = "GET")]
    request_method: String,
}

impl Cli {
    fn is_envvar_whitelisted(&self, var_name: &str) -> bool {
        if self.env_full {
            return true;
        }

        if !self.env_clear {
            if var_name.starts_with("HTTP_") || CGI_META_VARS.contains(&var_name) {
                return true;
            }
        }

        self.env_vars.contains(&String::from(var_name))
    }

    fn resolve_output_path(&self, path: impl AsRef<Path>) -> PathBuf {
        if let Some(output_directory) = self.output_directory.as_ref() {
            path.as_ref().join(output_directory)
        } else {
            path.as_ref().to_path_buf()
        }
    }

    fn real_output_file_name(&self) -> Result<Option<PathBuf>> {
        Ok(
            if self.output_file_remote_name {
                let url = self.url.as_ref().unwrap(); // cli should have caught this
                let last_path_segment = url.path_segments().unwrap().into_iter().last().ok_or(anyhow!("Remote file name has no length!"))?;
                Some(PathBuf::from(last_path_segment))
            } else {
                self.output_file_name.clone()
            }
        )
    }
}

trait ParamsExt<'a> {
    fn set_from_cli(self, cli: &Cli) -> Self;
    fn set_from_env<I, S1, S2>(self, vars: I) -> Self
        where
            I: IntoIterator<Item = (S1, S2)>,
            S1: Into<Cow<'a, str>>,
            S2: Into<Cow<'a, str>>;
}

impl<'a> ParamsExt<'a> for Params<'a> {
    fn set_from_cli(mut self, cli: &Cli) -> Self {
        self = self.request_method(cli.request_method.clone());

        let script_name =
            if let Some(sn) = cli.script_name.as_ref() { 
                self = self.script_name(sn.clone());
                sn
            } else {
                self.get("SCRIPT_NAME").map(|c| { c.as_ref() }).unwrap_or_default()
            }.to_string();

        if !script_name.is_empty() {
            if let Some(root) = cli.server_document_root.as_ref() {
                self = self.script_filename(root.to_string() + script_name.as_str())
            }
        }

        if let Some(url) = cli.url.as_ref() {
            let path_info = {
                let p = url.path();
                p.strip_prefix(script_name.as_str()).unwrap_or(p).to_string()
            };

            if !path_info.is_empty() {
                if let Some(root) = cli.server_document_root.as_ref() {
                    self.insert("PATH_TRANSLATED".into(), (root.to_owned() + path_info.as_str()).into());
                }
                self.insert("PATH_INFO".into(), path_info.into());
            }

            if let Some(Host::Domain(domain)) = url.host() {
                self.insert("HTTP_HOST".into(), domain.to_string().into());
            }

            if let Some(qs) = url.query() {
                self = self
                    .query_string(qs.to_string())
                    .request_uri(format!("{}?{}", url.path(), qs));
            } else {
                self = self.request_uri(url.path().to_string());
            }

            if url.scheme() == "https" {
                self.insert("HTTPS".into(), "on".into());
            }
        };

        self
    }

    fn set_from_env<I, S1, S2>(mut self, vars: I) -> Self
        where
            I: IntoIterator<Item = (S1, S2)>,
            S1: Into<Cow<'a, str>>,
            S2: Into<Cow<'a, str>>
    {
        self.extend(vars.into_iter().map(|t| { (t.0.into(), t.1.into()) }));
        self
    }
}

#[tokio::main]
async fn main() -> ExitCode {
    let cli = Cli::parse();

    if let Err(e) = execute(&cli).await {
        eprintln!("{}", e);
        ExitCode::FAILURE
    } else {
        ExitCode::SUCCESS
    }
}

async fn execute(cli: &Cli) -> Result<()> {
    let params = Params::default()
        .set_from_env(env::vars().filter_map(|envvar| {
                if cli.is_envvar_whitelisted(&envvar.0) {
                    Some((envvar.0, envvar.1))
                } else {
                    None
                }
            }))
        .set_from_cli(&cli);

    let input_stream = Box::<dyn io::AsyncRead>::into_pin(
        if let Some(data) = cli.data.as_ref() {
            Box::new(data.as_bytes())
        } else {
            if cli.request_method != "GET" {
                Box::new(io::stdin())
            } else {
                Box::new(io::empty())
            }
        }
    );

    let response =
        // No way to get this DRY....
        if !cli.address.contains('/') && cli.address.contains(':') {
            let stream = TcpStream::connect(&cli.address).await?;
            let client = Client::new(stream);
            client.execute_once(Request::new(params, input_stream)).await
        } else {
            let stream = UnixStream::connect(&cli.address).await?;
            let client = Client::new(stream);
            client.execute_once(Request::new(params, input_stream)).await
        }?;
    
    if let Some(data) = response.stdout {
        let mut out_stream = Box::<dyn io::AsyncWrite>::into_pin(
            if let Some(file_name) = cli.real_output_file_name()? {
                Box::new(OpenOptions::new()
                    .write(true)
                    .open(cli.resolve_output_path(file_name))
                    .await?
                )    
            } else {
                Box::new(io::stdout())
            }
        );

        io::copy(&mut data.as_slice(), &mut out_stream).await?;
    }

    if let Some(data) = response.stderr {
        let mut err_stream = Box::<dyn io::AsyncWrite>::into_pin(
            if let Some(file_name) = cli.stderr_file_name.as_ref() {
                Box::new(OpenOptions::new()
                    .write(true)
                    .open(cli.resolve_output_path(file_name))
                    .await?
                )
            } else {
                Box::new(io::stderr())
            }
        );

        io::copy(&mut data.as_slice(), &mut err_stream).await?;
    }

    Ok(())
}
