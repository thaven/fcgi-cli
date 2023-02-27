use anyhow::{anyhow, bail, Context, Result};
use clap::Parser;
use fastcgi_client::{Client, Params, Request};
use headers::parse_headers;
use std::{
    borrow::Cow,
    env,
    path::{Path, PathBuf},
    pin::Pin,
    process::ExitCode
};
use tokio::{
    fs::OpenOptions,
    io,
    net::{TcpStream, UnixStream}
};
use url::{Host, Url};

mod headers;

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

    /// Set the document root
    ///
    /// PATH should be a valid absolute path at the server, without trailing slash.
    #[arg(long = "root", value_name = "PATH")]
    server_document_root: Option<String>,

    /// Set the SCRIPT_NAME parameter
    #[arg(long = "script")]
    script_name: Option<String>,

    /// Send environment variable VAR as FastCGI parameter
    #[arg(short = 'e', long = "pass-env", value_name = "VAR")]
    env_vars: Vec<String>,

    /// Pass only excplicitly whitelisted environment variables
    ///
    /// Use -e, --pass-env to whitelist an environment variable
    #[arg(long = "no-env")]
    env_clear: bool,

    /// Pass all environment variables unmodified
    ///
    /// Default behavior is to pass only CGI-defined metavariables and protocol variables.
    #[arg(short = 'E', long = "full-env", conflicts_with = "env_clear")]
    env_full: bool,

    /// Dump response headers to file
    ///
    /// This option requires the headers to be parsed, in order to split the
    /// headers from the body.
    /// When dealing with malformed headers, refer to -i, --include.
    #[arg(short = 'D', long = "dump-header", value_name = "FILE")]
    response_headers_dump_file: Option<PathBuf>,

    /// Include response headers in output
    ///
    /// Unless required by other options, header parsing is disabled.
    /// Thus, this option allows you to dump malformed headers.
    #[arg(short = 'i', long = "include")]
    response_headers_include: bool,

    /// Fail and ignore the response body if the 'Status' header contains a value >= 400
    #[arg(short = 'f', long = "fail")]
    response_status_fail_on_gte_400: bool,

    /// Write ouput files to DIR
    #[arg(long = "output-dir", value_name = "DIR")]
    output_directory: Option<PathBuf>,

    /// Send output to specified file
    #[arg(short = 'o', long = "output", value_name = "FILE", conflicts_with = "output_file_remote_name")]
    output_file_name: Option<PathBuf>,

    /// Use the final segment of the URL path as output filename
    #[arg(short = 'O', long = "remote-name", requires = "url")]
    output_file_remote_name: bool,

    /// Send output received on the FCGI_STDERR stream to specified file.
    ///
    /// Error output generated locally will still be written to actual stderr.
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

    fn need_parse_header(&self) -> bool {
        self.response_status_fail_on_gte_400
            || !self.response_headers_include
            || self.response_headers_dump_file.is_some()
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

        if let Some(data) = cli.data.as_ref() {
            if self.get("CONTENT_LENGTH").is_none() {
                self = self.content_length(data.len());
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
    
    if let Some(data) = response.stdout.as_ref().map(Vec::as_slice) {
        handle_response_stdout(&cli, data).await?; // TODO: gently handle errors
    };

    if let Some(data) = response.stderr {
        handle_response_stderr(&cli, data).await?; // TODO: gently handle errors
    };

    Ok(())
}

async fn open_output_file(cli: &Cli, file_name: impl AsRef<Path>) -> io::Result<Pin<Box<dyn io::AsyncWrite>>> {
    Ok(Box::pin(
        OpenOptions::new()
            .write(true)
            .create(true)
            .truncate(true)
            .open(cli.resolve_output_path(file_name))
            .await?
    ))
}

async fn handle_response_stdout(cli: &Cli, data: &[u8]) -> Result<()> {
    let mut out = if cli.need_parse_header() {
        let (body, headers) = parse_headers(data)
            .map_err(|_e| anyhow!("Malformed response header."))?;

        if cli.response_status_fail_on_gte_400 {
            let status = headers
                .get("status")
                .map_or_else(|| { Ok(200u16) }, |s| {
                    let first_part = s.split_ascii_whitespace().next().unwrap_or("");
                    str::parse::<u16>(first_part)
                })
                .context("While parsing response header 'Status'")?;

            if status > 400 {
                bail!("Service returned an error response (code: {})", status);
            }
        };

        if let Some(file_name) = cli.response_headers_dump_file.as_ref() {
            let mut hdr_stream = open_output_file(&cli, file_name).await?;
            let hdr_len = data.len() - body.len();
            io::copy(&mut &data[..hdr_len], &mut hdr_stream).await?;
        }

        if cli.response_headers_include {
            data
        } else {
            body
        }
    } else {
        data
    };

    let mut out_stream: Pin<Box<dyn io::AsyncWrite>> =
        if let Some(file_name) = cli.real_output_file_name()? {
            open_output_file(&cli, file_name).await?
        } else {
            Box::pin(io::stdout())
        };

    io::copy(&mut out, &mut out_stream).await?;

    Ok(())
}

async fn handle_response_stderr(cli: &Cli, data: Vec<u8>) -> Result<()> {
    let mut err_stream: Pin<Box<dyn io::AsyncWrite>> =
    if let Some(file_name) = cli.stderr_file_name.as_ref() {
        open_output_file(&cli, file_name).await?
    } else {
        Box::pin(io::stderr())
    };

    io::copy(&mut data.as_slice(), &mut err_stream).await?;

    Ok(())
}