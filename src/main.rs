use axum::{
    body::{self, Body, Bytes, HttpBody},
    http::{header, HeaderValue},
    response::{Html, IntoResponse, Response},
};
use axum::{routing, Router};
use cargo_metadata::{Metadata, MetadataCommand};
use clap::{Parser, ValueEnum};
use color_eyre::eyre;
use std::net::{Ipv4Addr, SocketAddrV4};
use std::pin::Pin;
use std::sync::Arc;

#[derive(Parser, Debug)]
struct Options {
    #[clap(subcommand)]
    pub command: Command,
}

#[derive(clap::Subcommand, Debug)]
enum Command {
    /// Watchdoc
    Watchdoc(WatchdocOptions),
}

#[derive(Parser, Debug)]
#[command(bin_name = "cargo", version, about, author)]
struct WatchdocOptions {
    /// Opens docs in webbrowser.
    ///
    /// The given package is opened, or the root package otherwise.
    #[arg(
        short, long, num_args = 0..=1,
        default_missing_value = "crate",
        value_name = "PACKAGE",
    )]
    open: Option<String>,

    /// Clears terminal between runs
    #[arg(short, long)]
    clear: bool,

    /// Forces theme
    #[arg(short, long)]
    theme: Option<Theme>,

    /// Arguments after `--` are passed to `cargo doc`
    #[arg(allow_hyphen_values = true, last = true)]
    cargo_doc_args: Vec<String>,
}

#[derive(ValueEnum, Clone, Debug, Copy)]
enum Theme {
    Light,
    Dark,
    Ayu,
    AutoAyu,
    AutoDark,
}

async fn globset_filterer(
    root: &std::path::Path,
) -> eyre::Result<watchexec_filterer_globset::GlobsetFilterer> {
    let main_separator = std::path::MAIN_SEPARATOR;
    let ignores = [
        // Mac
        format!("*{main_separator}.DS_Store"),
        // Vim
        "*.sw?".into(),
        "*.sw?x".into(),
        // Emacs
        "#*#".into(),
        ".#*".into(),
        // Kate
        ".*.kate-swp".into(),
        // VCS
        format!("*{main_separator}.hg{main_separator}**"),
        format!("*{main_separator}.git{main_separator}**"),
        format!("*{main_separator}.svn{main_separator}**"),
        // SQLite
        "*.db".into(),
        "*.db-*".into(),
        format!("*{main_separator}*.db-journal{main_separator}**"),
        // Rust
        format!("*{main_separator}target{main_separator}**"),
        "rustc-ice-*.txt".into(),
    ];
    log::debug!("default ignores: {ignores:?}");

    let ignores = ignores.into_iter().map(|p| {
        (
            p, // None treats pattern `p` as global
            None,
        )
    });
    let ignore_files = ignore_files::from_origin(root)
        .await
        .0
        .into_iter()
        .chain(ignore_files::from_environment(None).await.0);

    let filters = [];
    let whitelist = [];
    let extensions = [];
    let filterer = watchexec_filterer_globset::GlobsetFilterer::new(
        &root,
        filters,
        ignores,
        whitelist,
        ignore_files,
        extensions,
    )
    .await?;

    Ok(filterer)
}

fn root_package(open: Option<&str>, metadata: &Metadata) -> Option<String> {
    open.and_then(|o| (o != "crate").then_some(o))
        .or_else(|| {
            metadata
                .root_package()
                .or_else(|| metadata.workspace_packages().first().copied())
                .map(|package| package.name.as_str())
        })
        .map(|package| package.replace('-', "_"))
}

fn open_in_browser(addr: &str, browser: Option<&cargo_config2::PathAndArgs>) -> eyre::Result<()> {
    if let Some(browser) = browser {
        std::process::Command::new(&browser.path)
            .args(&browser.args)
            .arg(addr)
            .spawn()?;
    } else {
        opener::open(addr)?;
    }
    Ok(())
}

fn build_app(
    metadata: &Metadata,
    _theme: Option<Theme>,
    root_addr: String,
) -> (Router, tower_livereload::Reloader) {
    let livereload = tower_livereload::LiveReloadLayer::new();
    let reloader = livereload.reloader();
    let app = Router::new().fallback_service(
        tower_http::services::ServeDir::new(metadata.target_directory.join("doc"))
            .not_found_service(routing::get(move || async move {
                Html(unindent::unindent(&format!(
                    r"
                        <head>
                            <meta http-equiv='refresh' content='0; URL={root_addr}'>
                        </head>
                    "
                )))
            })),
    );
    let app = app.layer(livereload);
    // if let Some(theme) = theme {
    //     app = app.layer(middleware::map_response(
    //         move |response: Response| async move { inject_theme_setter(response, theme) },
    //     ));
    // }
    (app, reloader)
}

#[derive(Parser, Debug, Clone)]
pub struct CargoDocOptions {
    #[clap(short = 'p', long = "package", help = "cargo workspace package")]
    pub package: Option<String>,
}

fn package_arg(args: &[String]) -> Option<String> {
    let options = CargoDocOptions::try_parse_from(
        [std::env!("CARGO_BIN_NAME")]
            .into_iter()
            .chain(args.iter().map(String::as_str)),
    )
    .ok()?;
    options.package
}

async fn run() -> eyre::Result<()> {
    stderrlog::new().init()?;
    let Options {
        command: Command::Watchdoc(options),
    } = Options::parse();

    let config = cargo_config2::Config::load()?;
    let metadata = MetadataCommand::new().exec()?;

    let port = if portpicker::is_free(4153) {
        4153
    } else {
        portpicker::pick_unused_port()
            .ok_or_else(|| eyre::eyre!("there should be an unused port left"))?
    };
    let addr = SocketAddrV4::new(Ipv4Addr::LOCALHOST, port);
    let listener = tokio::net::TcpListener::bind(addr).await?;

    let root_package = package_arg(&options.cargo_doc_args)
        .or_else(|| root_package(options.open.as_deref(), &metadata))
        .ok_or_else(|| {
            eyre::eyre!("project must have either a root package or workspace members")
        })?;
    let addr = format!("http://{addr}/{root_package}");
    eprintln!("Serving docs at: {addr}");

    let (app, reloader) = build_app(&metadata, options.theme, addr.clone());
    let app = axum::serve(listener, app.into_make_service());

    if options.open.is_some() {
        open_in_browser(&addr, config.doc.browser.as_ref())?;
    }

    let cargo_doc_command = Arc::new(watchexec::command::Command {
        program: watchexec::command::Program::Exec {
            prog: "cargo".into(),
            args: ["doc".into()]
                .into_iter()
                .chain(options.cargo_doc_args.clone())
                .collect(),
        },
        options: watchexec::command::SpawnOptions::default(),
    });

    let build_docs_id = watchexec::Id::default();
    let wx = watchexec::Watchexec::new_async(move |mut action| {
        let reloader_clone = reloader.clone();
        let cargo_doc_command_clone = Arc::clone(&cargo_doc_command);
        let addr = addr.clone();
        Box::new(async move {
            if action.signals().next().is_some() {
                eprintln!("received signal: quitting");
                action.quit();
                return action;
            }

            let build_docs =
                action.get_or_create_job(build_docs_id, || Arc::clone(&cargo_doc_command_clone));

            let start = action.events.iter().any(|event| event.tags.is_empty());
            let file_changed = action.paths().next().is_some();

            if file_changed || start {
                eprintln!("received path change");
                build_docs.restart().await;
            }

            build_docs.to_wait().await;
            build_docs
                .run(move |context| {
                    if let watchexec::job::CommandState::Finished {
                        status: watchexec_events::ProcessEnd::Success,
                        ..
                    } = context.current
                    {
                        eprintln!("reloading docs at {addr}");
                        reloader_clone.reload();
                    }
                })
                .await;

            action
        })
    })?;

    let root = metadata.workspace_root.as_std_path();
    let filterer = globset_filterer(root).await?;

    // and watch all files in the current directory:
    wx.config.pathset([root]);
    wx.config.filterer(filterer);

    // send an event to start
    wx.send_event(
        watchexec_events::Event::default(),
        watchexec_events::Priority::Urgent,
    )
    .await?;

    tokio::select! {
        wx= wx.main() => wx??,
        app = app => app?
    }

    Ok(())
}

#[allow(dead_code)]
fn inject_theme_setter(mut response: Response, theme: Theme) -> Response {
    use axum::Error;
    struct InjectBody(body::Body, Option<&'static str>);

    impl HttpBody for InjectBody {
        type Data = Bytes;
        type Error = Error;

        fn poll_frame(
            mut self: Pin<&mut Self>,
            cx: &mut std::task::Context<'_>,
        ) -> std::task::Poll<Option<Result<http_body::Frame<Self::Data>, Self::Error>>> {
            let poll = Pin::new(&mut self.0).poll_frame(cx);
            match poll {
                std::task::Poll::Ready(None) => {
                    std::task::Poll::Ready(self.1.take().map(|theme| {
                        Ok(http_body::Frame::data(Bytes::from_static(theme.as_bytes())))
                    }))
                }
                poll => poll,
            }
        }
    }

    macro_rules! theme_injection {
        ($dark:literal, $theme:literal, $auto:literal) => {
            concat!(
                r#"<script/> updateLocalStorage("preferred-dark-theme", ""#,
                $dark,
                r#""); updateLocalStorage("theme", ""#,
                $theme,
                r#""); updateLocalStorage("use-system-theme", ""#,
                $auto,
                r#""); updateTheme() </script>"#
            )
        };
    }

    let theme = match theme {
        Theme::Light => theme_injection!("dark", "light", "false"),
        Theme::Dark => theme_injection!("dark", "dark", "false"),
        Theme::Ayu => theme_injection!("ayu", "ayu", "false"),
        Theme::AutoAyu => theme_injection!("ayu", "ayu", "true"),
        Theme::AutoDark => theme_injection!("dark", "dark", "true"),
    };

    if response
        .headers()
        .get(header::CONTENT_TYPE)
        .is_some_and(|ct| ct.to_str().is_ok_and(|ct| ct.starts_with("text/html")))
    {
        response
            .headers_mut()
            .insert(header::CACHE_CONTROL, HeaderValue::from_static("no-store"));
        let (parts, body) = response.into_parts();
        Response::from_parts(parts, Body::new(InjectBody(body, Some(theme))))
    } else {
        response.into_response()
    }
}

#[tokio::main]
async fn main() -> eyre::Result<()> {
    run().await
}
