// SPDX-License-Identifier: MIT

use futures::TryStreamExt;
use nix::fcntl::{open, OFlag};
use nix::mount::{mount, MsFlags};
use nix::sched::{CloneFlags, unshare, setns};
use nix::unistd::{fork, ForkResult, Pid};
use nix::sys::wait::{waitpid, WaitStatus};
use nix::sys::stat::Mode;
use nix::sys::statvfs::{statvfs, FsFlags};
use rtnetlink::{new_connection, Error, Handle, NetworkNamespace};

use std::env;
use std::fs::{File, OpenOptions};
use std::path::{Path, PathBuf};
use std::process::exit;
use std::os::unix::io::RawFd;
use std::os::fd::FromRawFd;


static NETNS: &str = "/run/netns/";

#[tokio::main]
async fn main() -> Result<(), String> {

    env_logger::Builder::from_default_env()
        .format_timestamp_secs()
        .filter(None, log::LevelFilter::Debug)
        .init();

    let args: Vec<String> = env::args().collect();
    if args.len() != 2 {
        usage();
        return Ok(());
    }
    let ns_name = &args[1];
    run_in_namespace(ns_name).await.unwrap();

    Ok(())
}

pub async fn run_in_namespace(ns_name: &String) -> Result<(), ()> {
    prep_for_fork()?;
    // Configure networking in the child namespace:
    // Fork a process that is set to the newly created namespace
    // Here set the veth ip addr, routing tables etc.
    // Unfortunately the NetworkNamespace interface of rtnetlink does
    // not offer these functionalities
    match unsafe { fork() } {
        Ok(ForkResult::Parent { child, .. }) => {
            // Parent process
            log::debug!("Net configuration PID: {}", child.as_raw());
            run_parent(child)
        }
        Ok(ForkResult::Child) => {
            // Child process
            // Move the child to the target namespace
            run_child(ns_name).await
        }
        Err(e) => {
            log::error!("Can not fork() for ns creation: {}", e);
            return Err(());
        }
    }

}

fn run_parent(child: Pid) -> Result<(), ()> {
    log::trace!("[Parent] Child PID: {}", child);
    match waitpid(child, None) {
        Ok(wait_status) => match wait_status {
            WaitStatus::Exited(_, res) => {
                log::trace!("Child exited with: {}", res);
                if res == 0 {
                    return Ok(());
                } else {
                    log::error!("Child exited with status {}", res);
                    return Err(());
                }
            }
            WaitStatus::Signaled(_, signal, coredump) => {
                log::error!("Child process killed by signal");
                return Err(());
            }
            _ => {
                log::error!("Unknown child process status: {:?}", wait_status);
                return Err(());
            }
        }
        Err(e) => {
            log::error!("wait error : {}", e);
            return Err(());
        }
    }

}

async fn run_child(ns_name: &String) -> Result<(), ()> {
    let res = split_namespace(ns_name).await;

    match res {
        Err(_) => {
            log::error!("Child process crashed");
            std::process::abort()
        }
        Ok(()) => {
            log::debug!("Child exited normally");
            exit(0)
        }
    }
}

async fn split_namespace(ns_name: &String) -> Result<(), ()> {
    // First create the network namespace
    NetworkNamespace::add(ns_name.to_string()).await.map_err(|e| {
        log::error!("Can not create namespace {}", e);
    }).unwrap();

    // Open NS path
    let ns_path = format!("{}{}", NETNS, ns_name);

    let mut open_flags = OFlag::empty();
    open_flags.insert(OFlag::O_RDONLY);
    open_flags.insert(OFlag::O_CLOEXEC);

    let fd = match open(Path::new(&ns_path), open_flags, Mode::empty()) {
        Ok(raw_fd) => unsafe { 
            File::from_raw_fd(raw_fd)
        }
        Err(e) => {
            log::error!("Can not open network namespace: {}", e);
            return Err(());
        }
    };
    // Switch to network namespace with CLONE_NEWNET
    if let Err(e) = setns(fd, CloneFlags::CLONE_NEWNET) {
        log::error!("Can not set namespace to target {}: {}", ns_name, e);
        return Err(());
    }
    // unshare with CLONE_NEWNS
    if let Err(e) = unshare(CloneFlags::CLONE_NEWNS) {
        log::error!("Can not unshare: {}", e);
        return Err(());
    }
    // mount blind the fs
    // let's avoid that any mount propagates to the parent process
    // mount_directory(None, &PathBuf::from("/"), vec![MsFlags::MS_REC, MsFlags::MS_PRIVATE])?;
    let mut mount_flags = MsFlags::empty();
    mount_flags.insert(MsFlags::MS_REC);
    mount_flags.insert(MsFlags::MS_PRIVATE);
    if let Err(e) = mount::<PathBuf, PathBuf, str, PathBuf>(None, &PathBuf::from("/"), None, mount_flags, None) {
        log::error!("Can not remount root directory");
        ()
    }

    // Now unmount /sys
    let sys_path = PathBuf::from("/sys");
    mount_flags = MsFlags::empty();
    // Needed to respect the trait for NixPath
    let ns_name_path = PathBuf::from(ns_name);

    // TODO do not exit for EINVAL error
    // unmount_path(&sys_path)?;
    // consider the case that a sysfs is not present
    let stat_sys = statvfs(&sys_path)
        .map_err(|e| {
            log::error!("Can not stat sys: {}", e);
    }).unwrap();
    if stat_sys.flags().contains(FsFlags::ST_RDONLY) {
        mount_flags.insert(MsFlags::MS_RDONLY);
    }

    // and remount a version of /sys that describes the network namespace
    if let Err(e) = mount::<PathBuf, PathBuf, str, PathBuf>(Some(&ns_name_path), &sys_path, Some("sysfs"), mount_flags, None) {
        log::error!("Can not remount /sys to namespace: {}", e);
        ()
    }

    set_lo_up().await.unwrap();

    Ok(())
}

async fn set_lo_up() -> Result<(), Error> {
    let (connection, handle, _) = new_connection().unwrap();
    log::debug!("ARE WE STOPPING YET???");
    let veth_idx = handle.link().get().match_name("lo".to_string()).execute().try_next().await?
                .ok_or_else(|| log::error!("Can not find lo interface ")).unwrap()
                .header.index;
    log::debug!("LO INTERFACE INDEX: {}", veth_idx);
    handle.link().set(veth_idx).up().execute().await.unwrap();
    Ok(())
}


// Cargo cult from the definition in rtnetlink
fn prep_for_fork() -> Result<(), ()> {
    Ok(())
}

fn usage() {
    eprintln!(
        "usage: add_netns <ns_name>"
    );
}

