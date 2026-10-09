//! Corral's systemd units, generated from `corral.conf` and `helpers.ini` so ports, paths and helpers always match
//! the settings. Nothing here writes files; `corral install` does.
//!
//! - User units: `corral.target` (armed), the router, the guard, and each helper's socket, proxy and server.
//! - `corral web` and its socket, which holds every door: user units, or with a custom `name` system units that also
//!   hold the name's ports 80 and 443 and run `corral web` as the user.

use std::fmt::Write;

use crate::config::Config;
use crate::helpers::{Helper, Kind};

/// Everything the units depend on that differs between machines.
pub struct Context<'a> {
    pub config: &'a Config,
    pub helpers: &'a [Helper],
    /// The installed `corral` binary.
    pub corral: String,
    pub llama_server: String,
    pub socket_proxyd: String,
    pub config_dir: String,
    /// Set when the user's config doesn't live in `~/.config`, so the web front looks in the same place.
    pub config_home: Option<String>,
    pub home: String,
    pub user: String,
    pub group: String,
    /// Whether `ui.json` exists, so the router passes it to the web app.
    pub has_ui_defaults: bool,
}

pub struct Unit {
    pub name: String,
    pub content: String,
}

fn unit(name: impl Into<String>, content: String) -> Unit {
    Unit {
        name: name.into(),
        content,
    }
}

/// Quotes a command-line word for `ExecStart=` when needed, and escapes `%`, which systemd reads as a specifier.
fn quote(word: &str) -> String {
    let word = word.replace('%', "%%");
    if word.is_empty() || word.contains(|c: char| c.is_whitespace() || c == '"' || c == '\\' || c == '\'') {
        format!("\"{}\"", word.replace('\\', "\\\\").replace('"', "\\\""))
    } else {
        word
    }
}

fn command(words: &[String]) -> String {
    words.iter().map(|word| quote(word)).collect::<Vec<_>>().join(" ")
}

fn listen_lines(port: u16, all: bool) -> String {
    if all {
        format!("ListenStream={port}\n")
    } else {
        format!("ListenStream=127.0.0.1:{port}\nListenStream=[::1]:{port}\n")
    }
}

pub fn user_units(context: &Context) -> Vec<Unit> {
    let config = context.config;
    let router = config.router_port();
    // Active while Corral is armed; `corral web` starts the router on the first request and stops it after idle
    let mut units = vec![unit(
        "corral.target",
        "[Unit]\nDescription=Corral: armed (the router starts on the first request and stops after idle)\n".to_owned(),
    )];
    if config.name().is_none() {
        units.extend(web_units(context));
    }

    let mut router_command = vec![
        context.llama_server.clone(),
        "--models-preset".to_owned(),
        format!("{}/models.ini", context.config_dir),
        "--models-max".to_owned(),
        config.models_max().to_string(),
        "--metrics".to_owned(),
    ];
    if context.has_ui_defaults {
        router_command.extend(["--ui-config-file".to_owned(), format!("{}/ui.json", context.config_dir)]);
    }
    router_command.extend(config.router_options());
    router_command.extend(["--host", "127.0.0.1", "--port"].map(str::to_owned));
    router_command.push(router.to_string());
    units.push(unit(
        "corral-router.service",
        format!(
            "[Unit]\nDescription=Corral: model router (llama-server on 127.0.0.1:{router})\n\
             PartOf=corral.target\nWants=corral-guard.service\n\n[Service]\nExecStart={}\n\
             ExecStartPost={} ready {router}\nTimeoutStartSec=60\n",
            command(&router_command),
            quote(&context.corral)
        ),
    ));
    units.push(unit(
        "corral-guard.service",
        format!(
            "[Unit]\nDescription=Corral: guard (page cache, helpers, GPU memory)\nBindsTo=corral-router.service\n\
             After=corral-router.service\nPartOf=corral.target\n\n[Service]\nExecStart={} guard\n\
             Restart=on-failure\nRestartSec=5\n",
            quote(&context.corral)
        ),
    ));

    for helper in context.helpers {
        units.extend(helper_units(helper, context));
    }
    units
}

fn proxy(name: &str, service: &str, port: u16, context: &Context) -> String {
    format!(
        "[Unit]\nDescription=Corral: {name} proxy (exits after 15 idle minutes)\nRequires={service}\n\
         After={service}\nPartOf=corral.target\n\n[Service]\nExecStart={} --exit-idle-time=15min 127.0.0.1:{port}\n",
        quote(&context.socket_proxyd)
    )
}

fn helper_units(helper: &Helper, context: &Context) -> [Unit; 3] {
    let name = &helper.name;
    let mut words = helper.command_line(&context.home);
    // `ready` waits for llama-server's health check; any other server is ready once it accepts connections
    let ready = match &helper.kind {
        Kind::Llama { .. } => {
            words[0].clone_from(&context.llama_server);
            format!("{} ready {}", quote(&context.corral), helper.internal_port)
        }
        Kind::Custom { .. } => format!("{} ready {} --connect", quote(&context.corral), helper.internal_port),
    };
    [
        unit(
            helper.socket_unit(),
            format!(
                "[Unit]\nDescription=Corral: {name} (port {})\nPartOf=corral.target\n\n[Socket]\n{}Service={}\n",
                helper.port,
                listen_lines(helper.port, false),
                helper.proxy_unit()
            ),
        ),
        unit(
            helper.proxy_unit(),
            proxy(name, &helper.service_unit(), helper.internal_port, context),
        ),
        unit(
            helper.service_unit(),
            format!(
                "[Unit]\nDescription=Corral: {name} (127.0.0.1:{})\nStopWhenUnneeded=yes\nPartOf=corral.target\n\n\
                 [Service]\nExecStart={}\nExecStartPost={ready}\nTimeoutStartSec=90\n",
                helper.internal_port,
                command(&words)
            ),
        ),
    ]
}

/// The web front's system units, only with a custom `name` (they bind its ports 80 and 443, which needs root).
pub fn system_units(context: &Context) -> Vec<Unit> {
    if context.config.name().is_some() {
        web_units(context)
    } else {
        Vec::new()
    }
}

/// `corral web` and the socket that holds every door: the gate port always, and with a custom name its ports 80 and
/// 443 too. Without a name these are user units; with one, system units that run `corral web` as the user. The socket
/// stays open while Corral is off, so the gate answers "off" instead of refusing.
pub fn web_units(context: &Context) -> Vec<Unit> {
    let config = context.config;
    let gate = config.gate_port();
    let mut doors = listen_lines(gate, config.listen_on_all());
    if config.name().is_some() {
        let address = config.name_address();
        let _ = write!(doors, "ListenStream={address}:80\nListenStream={address}:443\n");
    }
    let description = config.name().map_or_else(
        || format!("the gate (port {gate})"),
        |name| format!("the gate (port {gate}) and https://{name}"),
    );

    let mut service =
        format!("[Unit]\nDescription=Corral web front: {description}\nRequires=corral-web.socket\n\n[Service]\n");
    if config.name().is_some() {
        let _ = writeln!(service, "User={}\nGroup={}", context.user, context.group);
    }
    let _ = writeln!(service, "ExecStart={} web", quote(&context.corral));
    if let Some(config_home) = &context.config_home {
        let _ = writeln!(service, "Environment=XDG_CONFIG_HOME={}", quote(config_home));
    }
    service.push_str("NoNewPrivileges=yes\nPrivateTmp=yes\nRestrictAddressFamilies=AF_INET AF_INET6 AF_UNIX\n");
    if config.name().is_some() {
        service.push_str("CapabilityBoundingSet=\nProtectSystem=strict\nProtectHome=read-only\nPrivateDevices=yes\n");
    }
    vec![
        unit(
            "corral-web.socket",
            format!(
                "[Unit]\nDescription=Corral: {description}\n\n[Socket]\n{doors}\n[Install]\nWantedBy=sockets.target\n"
            ),
        ),
        unit("corral-web.service", service),
    ]
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::helpers;

    fn context<'a>(config: &'a Config, helpers: &'a [Helper]) -> Context<'a> {
        Context {
            config,
            helpers,
            corral: "/home/me/.local/bin/corral".to_owned(),
            llama_server: "/usr/bin/llama-server".to_owned(),
            socket_proxyd: "/usr/lib/systemd/systemd-socket-proxyd".to_owned(),
            config_dir: "/home/me/.config/corral".to_owned(),
            config_home: None,
            home: "/home/me".to_owned(),
            user: "me".to_owned(),
            group: "me".to_owned(),
            has_ui_defaults: true,
        }
    }

    fn find<'a>(units: &'a [Unit], name: &str) -> &'a str {
        &units.iter().find(|unit| unit.name == name).expect(name).content
    }

    #[test]
    fn the_gate_is_local_unless_set_to_all() {
        let local = Config::parse("");
        let units = user_units(&context(&local, &[]));
        let socket = find(&units, "corral-web.socket");
        assert!(socket.contains("ListenStream=127.0.0.1:24937\nListenStream=[::1]:24937"));
        assert!(!units.iter().any(|unit| unit.name == "corral-router.socket"));
        assert!(!find(&units, "corral-web.service").contains("User="));
        let all = Config::parse("listen = all\ngate_port = 9000");
        assert!(find(&user_units(&context(&all, &[])), "corral-web.socket").contains("ListenStream=9000\n"));
    }

    #[test]
    fn the_router_gets_its_settings() {
        let config = Config::parse("router_options = --tools read_file,get_datetime\nmodels_max = 2");
        let units = user_units(&context(&config, &[]));
        let router = find(&units, "corral-router.service");
        assert!(router.contains(
            "ExecStart=/usr/bin/llama-server --models-preset /home/me/.config/corral/models.ini --models-max 2 \
             --metrics --ui-config-file /home/me/.config/corral/ui.json --tools read_file,get_datetime \
             --host 127.0.0.1 --port 18080\n"
        ));
        assert!(router.contains("ExecStartPost=/home/me/.local/bin/corral ready 18080\n"));
        assert!(find(&units, "corral-guard.service").contains("ExecStart=/home/me/.local/bin/corral guard\n"));
    }

    #[test]
    fn helpers_get_three_units() {
        let (helpers, _) = helpers::parse("[sweep]\nmodel = ~/Models/sweep.gguf\nctx-size = 8192\nport = 8081\n");
        let config = Config::parse("");
        let units = user_units(&context(&config, &helpers));
        assert!(find(&units, "corral-sweep.socket").contains("Service=corral-sweep-proxy.service"));
        assert!(find(&units, "corral-sweep-proxy.service").contains("127.0.0.1:18081"));
        assert!(find(&units, "corral-sweep.service").contains(
            "ExecStart=/usr/bin/llama-server --ctx-size 8192 --model /home/me/Models/sweep.gguf --host 127.0.0.1 \
             --port 18081 --no-webui\n"
        ));
    }

    #[test]
    fn system_units_only_with_a_name() {
        let config = Config::parse("");
        assert!(system_units(&context(&config, &[])).is_empty());
        let named = Config::parse("name = corral.example");
        let units = system_units(&context(&named, &[]));
        let socket = find(&units, "corral-web.socket");
        assert!(socket.contains("ListenStream=127.0.0.1:24937\nListenStream=[::1]:24937\n"));
        assert!(socket.contains("ListenStream=127.0.97.105:80\nListenStream=127.0.97.105:443"));
        assert!(
            !user_units(&context(&named, &[]))
                .iter()
                .any(|unit| unit.name.starts_with("corral-web"))
        );
        assert!(
            find(&units, "corral-web.service").contains("User=me\nGroup=me\nExecStart=/home/me/.local/bin/corral web")
        );
    }

    #[test]
    fn quoting_for_exec_start() {
        assert_eq!(quote("plain"), "plain");
        assert_eq!(quote("/a path/x"), "\"/a path/x\"");
        assert_eq!(quote("{\"a\": 1}"), "\"{\\\"a\\\": 1}\"");
        assert_eq!(quote("100%"), "100%%");
    }
}
