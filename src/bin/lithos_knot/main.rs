extern crate argparse;
extern crate base64;
extern crate blake2;
extern crate humantime;
extern crate ipnetwork;
extern crate libc;
extern crate libmount;
extern crate lithos;
extern crate nix;
extern crate quire;
extern crate serde_json;
extern crate signal;
extern crate syslog;
extern crate ssh_keys;
extern crate unshare;
#[macro_use] extern crate failure;
#[macro_use] extern crate log;
#[macro_use] extern crate serde_derive;

use std::env;
use std::str::FromStr;
use std::io::{stderr, Write};
use std::fs::OpenOptions;
use std::path::{Path};
use std::time::{SystemTime, Instant, Duration};
use std::thread::sleep;
use std::process::exit;
use std::net::SocketAddr;

use humantime::format_rfc3339_seconds;
use libmount::BindMount;
use quire::{parse_config, Options as COptions};
use signal::trap::Trap;
use unshare::{Command, Stdio, Style, reap_zombies, Capability, Namespace};
use nix::sys::signal::Signal;
use nix::sys::signal::{SIGINT, SIGTERM, SIGCHLD};
use nix::sys::socket::{InetAddr, SockAddr};

use lithos::cgroup;
use lithos::utils::{check_mapping, in_mapping, change_root};
use lithos::utils::{temporary_change_root};
use lithos::range::in_range;
use lithos::master_config::MasterConfig;
use lithos::sandbox_config::SandboxConfig;
use lithos::container_config::{ContainerConfig, Variables};
use lithos::container_config::ContainerKind::Daemon;
use lithos::setup::{init_logging};
use lithos::mount::{unmount, mount_private, mount_ro_recursive, mount_pseudo};
use lithos::limits::{set_fileno_limit};
use lithos::knot_options::Options;

use setup_filesystem::{setup_filesystem, prepare_state_dir};

mod setup_network;
mod setup_filesystem;
mod config;
mod secrets;

struct SignalIter<'a> {
    trap: &'a mut Trap,
    deadline: Option<Instant>,
}

impl<'a> SignalIter<'a> {
    fn new(trap: &mut Trap) -> SignalIter {
        SignalIter {
            trap: trap,
            deadline: None,
        }
    }
    fn set_deadline(&mut self, time: Instant) {
        self.deadline = Some(time);
    }
    fn interrupt(&mut self) {
        self.deadline = Some(Instant::now());
    }
}

impl<'a> Iterator for SignalIter<'a> {
    type Item = Signal;
    fn next(&mut self) -> Option<Signal> {
        if let Some(dline) = self.deadline {
            return self.trap.wait(dline);
        } else {
            return self.trap.next();
        }
    }
}

fn duration(inp: f32) -> Duration {
    Duration::from_millis((inp * 1000.) as u64)
}

fn run(options: &Options) -> Result<i32, String>
{
    let master: MasterConfig = try!(parse_config(&options.master_config,
        &MasterConfig::validator(), &COptions::default())
        .map_err(|e| format!("Error reading master config: {}", e)));
    let sandbox_name = options.name[..].splitn(2, '/').next().unwrap();
    let sandbox: SandboxConfig = try!(parse_config(
        &options.master_config.parent().unwrap()
         .join(&master.sandboxes_dir).join(sandbox_name.to_string() + ".yaml"),
        &SandboxConfig::validator(), &COptions::default())
        .map_err(|e| format!("Error reading sandbox config: {}", e)));

    let log_file;
    if let Some(ref fname) = sandbox.log_file {
        log_file = master.default_log_dir.join(fname);
    } else {
        log_file = master.default_log_dir.join(format!("{}.log", sandbox_name));
    }
    try!(init_logging(&master, &log_file,
        &format!("{}-{}", master.syslog_app_name, sandbox_name),
        options.log_stderr,
        options.log_level
            .or(sandbox.log_level.as_ref()
                .and_then(|x| FromStr::from_str(&x).ok()))
            .or_else(|| FromStr::from_str(&master.log_level).ok())
            .unwrap_or(log::LogLevel::Warn)));

    let stderr_path = master.stdio_log_dir
        .join(format!("{}.log", sandbox_name));
    let mut stderr_file = try!(OpenOptions::new()
                .create(true).append(true).write(true).open(&stderr_path)
                .map_err(|e| format!(
                    "Error opening stderr file {:?}: {}", stderr_path, e)));

    try!(mount_private(&Path::new("/")));
    let image_path = sandbox.image_dir.join(&options.config.image);
    let mount_dir = master.runtime_dir.join(&master.mount_dir);
    try!(BindMount::new(&image_path, &mount_dir).mount()
        .map_err(|e| e.to_string()));
    try!(mount_ro_recursive(&mount_dir));

    let container: ContainerConfig;
    container = config::container_config(&mount_dir, &options.config)?;
    if !container.kind.matches(options.config.kind) {
        return Err(format!("Container type mismatch {:?} != {:?}",
              container.kind, options.config.kind));
    }
    let mut local = container.instantiate(&Variables {
        user_vars: &options.config.variables,
        lithos_name: &options.name,
        lithos_config_filename: &options.config.config,
    }).map_err(|e| format!("Variable substitution error: {}", e.join("; ")))?;

    let user_id = if
        let Some(user_id) = local.user_id.or(sandbox.default_user)
    {
        if local.uid_map.len() > 0 {
            if !in_mapping(&local.uid_map, user_id) {
                return Err(format!("User is not in mapped range (uid: {})",
                    user_id));
            }
        } else {
            if !in_range(&sandbox.allow_users, user_id) {
                return Err(format!("User is not in allowed range (uid: {})",
                    user_id));
            }
        }
        user_id
    } else {
        return Err(format!("No user id specified and no default is found"));
    };

    let group_id = if
        let Some(group_id) = local.group_id.or(sandbox.default_group)
    {
        if local.gid_map.len() > 0 {
            if !in_mapping(&local.gid_map, group_id) {
                return Err(format!("Group is not in mapped range (gid: {})",
                    group_id));
            }
        } else {
            if !in_range(&sandbox.allow_groups, group_id) {
                return Err(format!("Group is not in allowed range (gid: {})",
                    group_id));
            }
        }
        group_id
    } else {
        return Err(format!("No group id specified and no default is found"));
    };

    if !check_mapping(&sandbox.allow_users, &local.uid_map) {
        return Err("Bad uid mapping (probably doesn't match allow_users)"
            .to_string());
    }
    if !check_mapping(&sandbox.allow_groups, &local.gid_map) {
        return Err("Bad gid mapping (probably doesn't match allow_groups)"
            .to_string());
    }

    info!("[{}] Starting container", options.name);
    let state_dir = &master.runtime_dir.join(&master.state_dir)
        .join(&options.name);
    try!(prepare_state_dir(state_dir, &local, &sandbox));
    try!(setup_filesystem(&master, &sandbox, &local, state_dir));
    if let Some(cgroup_parent) = master.cgroup_name {
        // Warning setting cgroup relative to it's own cgroup may not work
        // if we ever want to restart lithos_knot in-place
        let cgroups = try!(cgroup::ensure_in_group(
            &(cgroup_parent + "/" +
              &options.name.replace("/", ":") + ".scope"),
            &master.cgroup_controllers));
        cgroups.set_value(cgroup::Controller::Memory,
            "memory.limit_in_bytes",
            &format!("{}", local.memory_limit))
            .map_err(|e| error!("Error setting cgroup limit: {}", e)).ok();
        cgroups.set_value_if_exists(cgroup::Controller::Memory,
            "memory.memsw.limit_in_bytes",
            &format!("{}", local.memory_limit))
            .map_err(|e| error!("Error setting cgroup limit: {}", e)).ok();
        cgroups.set_value(cgroup::Controller::Cpu,
                "cpu.shares",
                &format!("{}", local.cpu_shares))
            .map_err(|e| error!("Error setting cgroup limit: {}", e)).ok();
    }

    let has_secrets = container.secret_environ_file.is_some() ||
                      !container.secret_environ.is_empty();
    let keys = if has_secrets {
        Some(secrets::read_keys(&sandbox)
            .map_err(|e| format!("Error decoding private keys: {}", e))?)
    } else {
        None
    };

    if let Some(ref keys) = keys {
        temporary_change_root(&mount_dir, || {
            let senv = if let Some(ref path) = container.secret_environ_file {
                if !container.secret_environ.is_empty() {
                    return Err(format!("secret-environ and \
                        secret-environ-file \
                        settings are mutually exclusive"));
                }

                let path = Path::new(&options.config.config).parent()
                    .expect("file always have parent path").join(path);
                Some(secrets::parse_file(&path)
                    .map_err(|e| format!("Can't read secret \
                                         environ file {:?}: {}",
                                         path, e))?)
            } else {
                None
            };
            let secrets = secrets::decode(keys, &sandbox, &options.config,
                senv.as_ref().unwrap_or(&container.secret_environ))
                .map_err(|e| format!("Error decoding secrets: {}", e))?;
            local.environ.extend(secrets);
            Ok(())
        })?;
    }
    drop(keys);

    try!(set_fileno_limit(local.fileno_limit)
        .map_err(|e| format!("Error setting file limit: {}", e)));

    // This is needed for unshare to properly initialize user namespace
    mount_pseudo(&Path::new("/proc"), "proc", "", false)?;

    let mut cmd = Command::new(&local.executable);
    cmd.uid(user_id);
    cmd.gid(group_id);
    if sandbox.bridged_network.is_some() {
        cmd.keep_caps(&[
            Capability::CAP_NET_BIND_SERVICE,
        ]);
    }
    cmd.current_dir(&local.workdir);

    // Should we propagate TERM?
    cmd.env_clear();
    cmd.env("TERM", env::var("TERM").unwrap_or("dumb".to_string()));
    for (k, v) in local.environ.iter() {
        cmd.env(k, v);
    }
    cmd.env("LITHOS_NAME", &options.name);
    cmd.env("LITHOS_CONFIG", &options.config.config);
    for var in &local.pid_env_vars {
        cmd.env_var_with_pid(var);
    }

    cmd.args(&local.arguments);
    cmd.args(&options.args);
    if sandbox.uid_map.len() > 0 || sandbox.gid_map.len() > 0 {
        cmd.set_id_maps(
            sandbox.uid_map.iter().map(|u| unshare::UidMap {
                inside_uid: u.inside,
                outside_uid: u.outside,
                count: u.count,
            }).collect(),
            sandbox.gid_map.iter().map(|g| unshare::GidMap {
                inside_gid: g.inside,
                outside_gid: g.outside,
                count: g.count,
            }).collect());
    } else if local.uid_map.len() > 0 || local.gid_map.len() > 0 {
        cmd.set_id_maps(
            local.uid_map.iter().map(|u| unshare::UidMap {
                inside_uid: u.inside,
                outside_uid: u.outside,
                count: u.count,
            }).collect(),
            local.gid_map.iter().map(|g| unshare::GidMap {
                inside_gid: g.inside,
                outside_gid: g.outside,
                count: g.count,
            }).collect());
    }

    let mount_dir = master.runtime_dir.join(&master.mount_dir);
    let child_setup = move |_pid| {
        change_root(&mount_dir, &mount_dir.join("tmp"))?;
        unmount(Path::new("/tmp"))?;
        Ok(())
    };
    if let Some(ref net) = sandbox.bridged_network {
        cmd.unshare(&[Namespace::Net]);

        let net = net.clone();
        let child = options.config.clone();
        cmd.before_unfreeze(move |pid| {
            setup_network::setup(pid, &net, &child)?;
            child_setup(pid)?;
            Ok(())
        });
        let sockets = local.tcp_ports.iter()
            .filter(|(_, v)| !v.external)
            .map(|(port, cfg)| {
                let addr = SockAddr::new_inet(InetAddr::from_std(
                    &SocketAddr::new(cfg.host.0, *port)));
                (cfg.clone(), addr)
            })
            .collect::<Vec<_>>();
        cmd.before_exec(move || {
            for &(ref cfg, ref addr) in &sockets {
                unsafe {
                    setup_network::open_socket(cfg, addr)?;
                }
            }
            Ok(())
        });
    } else {
        cmd.before_unfreeze(child_setup);
    }
    let rtimeo = Duration::from_millis((local.restart_timeout*1000.0) as u64);

    let mut trap = Trap::trap(&[SIGINT, SIGTERM, SIGCHLD]);
    let mut should_exit = local.kind != Daemon || !local.restart_process_only;
    // only successful code on SIGTERM
    let mut exit_code = 2;
    loop {
        let start = Instant::now();
        let mut killed = false;
        let mut dead = false;

        if !local.interactive {
            if let Some(ref path) = local.stdout_stderr_file {
                // Reopen file at each start
                let f = try!(OpenOptions::new()
                    .create(true).append(true).write(true).open(path)
                    .map_err(|e| format!(
                        "Error opening output file {:?}: {}", path, e)));
                cmd.stdout(try!(Stdio::dup_file(&f)
                    .map_err(|e| format!(
                        "Duplicating file descriptor: {}", e))));
                cmd.stderr(Stdio::from_file(f));
            } else {
                // Can't reopen, because file is outside of container
                cmd.stdout(try!(Stdio::dup_file(&stderr_file)
                    .map_err(|e| format!(
                        "Duplicating file descriptor: {}", e))));
                cmd.stderr(try!(Stdio::dup_file(&stderr_file)
                    .map_err(|e| format!(
                        "Duplicating file descriptor: {}", e))));
            };
        }

        warn!("Starting {:?}: {}", options.name,
            cmd.display(&Style::short().path(true)));
        stderr_file.write_all(
            format!("{}: ----- Starting {:?}: {} -----\n",
                format_rfc3339_seconds(SystemTime::now()), options.name,
                cmd.display(&Style::short().path(true)))
            .as_bytes()
        ).ok();
        let child = try!(cmd.spawn().map_err(|e|
            format!("Error running {:?}: {}", options.name, e)));

        let mut iter = SignalIter::new(&mut trap);
        while let Some(signal) = iter.next() {
            match signal {
                SIGINT => {
                    // SIGINT is usually a Ctrl+C so it's sent to whole
                    // process group, so we don't need to do anything special
                    debug!("Received SIGINT. Waiting process to stop..");
                    should_exit = true;
                }
                SIGTERM => {
                    // SIGTERM is usually sent to a specific process so we
                    // forward it to children
                    debug!("Received SIGTERM signal, propagating");
                    should_exit = true;
                    exit_code = 0;
                    if !killed {
                        if let Ok(()) = child.signal(SIGTERM) {
                            killed = true;
                        }
                        iter.set_deadline(
                            Instant::now() + duration(container.kill_timeout));
                    }
                }
                SIGCHLD => {
                    for (pid, status) in reap_zombies() {
                        if pid == child.pid() {
                            dead = true;
                            if status.signal() == Some(SIGTERM as i32) ||
                                status.code().map(|c| {
                                    if container.normal_exit_codes.is_empty() {
                                        local.kind != Daemon && c == 0
                                    } else {
                                        container.normal_exit_codes.contains(&c)
                                    }
                                }).unwrap_or(false)
                            {
                                exit_code = 0;
                            }
                            let uptime = Instant::now() - start;
                            error!("Process {:?} {}, uptime {}s",
                                options.name, status, uptime.as_secs());
                            stderr_file.write_all(
                                format!("{}: ----- \
                                    Process {:?} {}, uptime {}s \
                                    -----\n",
                                    format_rfc3339_seconds(SystemTime::now()),
                                    options.name, status, uptime.as_secs(),
                                ).as_bytes()
                            ).ok();
                            iter.interrupt();
                        }
                    }
                }
                _ => unreachable!(),
            }
        }
        if !dead {
            let uptime = Instant::now() - start;
            error!("Process {:?} \
                did not respond to SIGTERM in {}s, uptime {}s. \
                Killing container so hanging process will die.",
                options.name, container.kill_timeout, uptime.as_secs());
            stderr_file.write_all(
                format!("{}: ----- \
                    Process {:?} did not respond to SIGTERM in {}, \
                    uptime {}s. Killing.. -----\n",
                    format_rfc3339_seconds(SystemTime::now()),
                    options.name, container.kill_timeout, uptime.as_secs(),
                ).as_bytes()
            ).ok();
            return Ok(3);
        }

        if should_exit {
            break;
        }
        let left = rtimeo - (Instant::now() - start);
        if left > Duration::new(0, 0) {
            sleep(left);
        }
    }

    Ok(exit_code)
}


fn main() {

    let options = match Options::parse_args() {
        Ok(options) => options,
        Err(x) => {
            exit(x);
        }
    };
    match run(&options)
    {
        Ok(code) => {
            exit(code);
        }
        Err(e) => {
            write!(&mut stderr(), "Fatal error: {}\n", e).ok();
            error!("Fatal error running {:?}: {}", options.name, e);
            exit(1);
        }
    }
}
