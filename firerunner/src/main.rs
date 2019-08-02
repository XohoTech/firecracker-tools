#[macro_use(crate_version, crate_authors)]
extern crate clap;
extern crate futures;
extern crate vmm;
extern crate sys_util;

use std::rc::Rc;
use std::sync::{Arc, RwLock};
use std::sync::mpsc::{channel, Sender};
use std::path::PathBuf;

use futures::Future;
use futures::sync::oneshot;
use clap::{App, Arg};
use vmm::{VmmAction, VmmActionError, VmmData};
use vmm::vmm_config::instance_info::{InstanceInfo, InstanceState};
use vmm::vmm_config::boot_source::BootSourceConfig;
use vmm::vmm_config::drive::BlockDeviceConfig;
use sys_util::EventFd;

fn main() {
    let cmd_arguments = App::new("firecracker")
        .version(crate_version!())
        .author(crate_authors!())
        .about("Launch a microvm.")
        .arg(
            Arg::with_name("from_file")
                .short("f")
                .long("from_file")
                .takes_value(false)
                .required(false)
                .help("Whether load vcpu's regs and sregs from regs_sregs or not")
        )
        .arg(
            Arg::with_name("kernel")
                .short("k")
                .long("kernel")
                .value_name("KERNEL")
                .takes_value(true)
                .required(true)
                .help("Path the the kernel binary")
        )
        .arg(
            Arg::with_name("command line")
                .short("c")
                .long("cmd_line")
                .value_name("CMD_LINE")
                .takes_value(true)
                .required(false)
                .default_value("console=ttyS0 reboot=k panic=1 pci=off nokaslr")
                .help("Command line to pass to the kernel")
        )
        .arg(
            Arg::with_name("rootfs")
                .long("r")
                .long("rootfs")
                .value_name("ROOTFS")
                .takes_value(true)
                .required(true)
                .help("Path to the root file system")
        )
        .arg(
            Arg::with_name("appfs")
                .long("a")
                .long("appfs")
                .value_name("APPFS")
                .takes_value(true)
                .required(false)
                .help("Path to the root file system")
        )
        .arg(
            Arg::with_name("id")
                .long("id")
                .help("MicroVM unique identifier")
                .default_value("abcde1234")
                .takes_value(true)
                ,
        )
        .arg(
            Arg::with_name("seccomp-level")
                .long("seccomp-level")
                .help(
                    "Level of seccomp filtering.\n
                            - Level 0: No filtering.\n
                            - Level 1: Seccomp filtering by syscall number.\n
                            - Level 2: Seccomp filtering by syscall number and argument values.\n
                        ",
                )
                .takes_value(true)
                .default_value("0")
                .possible_values(&["0", "1", "2"]),
        )
        .get_matches();

    let kernel = cmd_arguments.value_of("kernel").unwrap().to_string();
    let rootfs = cmd_arguments.value_of("rootfs").unwrap().to_string();
    let appfs = cmd_arguments.value_of("appfs");
    let cmd_line = cmd_arguments.value_of("command line").unwrap().to_string();
    if cmd_arguments.is_present("from_file") {
        unsafe { vmm::FROM_FILE = true };
        println!("load regs and sregs from regs_sregs");
    } else {
        println!("start from the beginning");
    }

    // It's safe to unwrap here because clap's been provided with a default value
    let instance_id = cmd_arguments.value_of("id").unwrap().to_string();

    // We disable seccomp filtering when testing, because when running the test_gnutests
    // integration test from test_unittests.py, an invalid syscall is issued, and we crash
    // otherwise.
    #[cfg(test)]
    let seccomp_level = seccomp::SECCOMP_LEVEL_NONE;
    #[cfg(not(test))]
    // It's safe to unwrap here because clap's been provided with a default value,
    // and allowed values are guaranteed to parse to u32.
    let seccomp_level = cmd_arguments
        .value_of("seccomp-level")
        .unwrap()
        .parse::<u32>()
        .unwrap();

    let shared_info = Arc::new(RwLock::new(InstanceInfo {
        state: InstanceState::Uninitialized,
        id: instance_id,
        vmm_version: crate_version!().to_string(),
    }));

    let (sender, recv) = channel();
    let event_fd = Rc::new(EventFd::new().expect("Cannot create EventFd"));

    let vmm_thread_handle =
        vmm::start_vmm_thread(shared_info.clone(), event_fd.try_clone().expect("Couldn't clone event_fd"), recv, seccomp_level);

    let mut vmm = VmmWrapper {
        sender,
        event_fd,
    };

    println!("Configuration: {:?}", vmm.get_configuration().expect("config"));

    let boot_config = BootSourceConfig {
        kernel_image_path: kernel,
        boot_args: Some(cmd_line),
    };
    println!("{:?}", vmm.set_boot_source(boot_config).expect("bootsource"));

    let block_config = BlockDeviceConfig {
        drive_id: String::from("rootfs"),
        path_on_host: PathBuf::from(rootfs),
        is_root_device: true,
        is_read_only: true,
        partuuid: None,
        rate_limiter: None,
    };
    println!("{:?}", vmm.insert_block_device(block_config).expect("Rootfs"));
    if let Some(appfs) = appfs {
        let block_config = BlockDeviceConfig {
            drive_id: String::from("appfs"),
            path_on_host: PathBuf::from(appfs),
            is_root_device: false,
            is_read_only: true,
            partuuid: None,
            rate_limiter: None,
        };
        println!("AppBlk {:?}", vmm.insert_block_device(block_config).expect("AppBlk"));
    }

    println!("Starting {:?}", vmm.start_instance().expect("Start"));
    println!("State {:?}", shared_info.read().expect("SharedInfo").state);
    vmm_thread_handle.join().expect("Join");
}

struct VmmWrapper {
    sender: Sender<Box<VmmAction>>,
    event_fd: Rc<EventFd>,
}

impl VmmWrapper {
    fn get_configuration(&mut self) -> Result<VmmData, VmmActionError> {
        let (sync_sender, sync_receiver) = oneshot::channel();
        let req = VmmAction::GetVmConfiguration(sync_sender);
        self.sender.send(Box::new(req)).map_err(|_| ()).expect("Couldn't send");
        self.event_fd.write(1).map_err(|_| ()).expect("Failed to signal");
        sync_receiver.map(|i| {
            i
        }).wait().unwrap()
    }

    fn set_boot_source(&mut self, config: BootSourceConfig) -> Result<VmmData, VmmActionError> {
        let (sync_sender, sync_receiver) = oneshot::channel();
        let req = VmmAction::ConfigureBootSource(config, sync_sender);
        self.sender.send(Box::new(req)).map_err(|_| ()).expect("Couldn't send");
        self.event_fd.write(1).map_err(|_| ()).expect("Failed to signal");
        sync_receiver.map(|i| {
            i
        }).wait().unwrap()
    }

    fn insert_block_device(&mut self, config: BlockDeviceConfig) -> Result<VmmData, VmmActionError> {
        let (sync_sender, sync_receiver) = oneshot::channel();
        let req = VmmAction::InsertBlockDevice(config, sync_sender);
        self.sender.send(Box::new(req)).map_err(|_| ()).expect("Couldn't send");
        self.event_fd.write(1).map_err(|_| ()).expect("Failed to signal");
        sync_receiver.map(|i| {
            i
        }).wait().unwrap()
    }

    fn start_instance(&mut self) -> Result<VmmData, VmmActionError> {
        let (sync_sender, sync_receiver) = oneshot::channel();
        let req = VmmAction::StartMicroVm(sync_sender);
        self.sender.send(Box::new(req)).map_err(|_| ()).expect("Couldn't send");
        self.event_fd.write(1).map_err(|_| ()).expect("Failed to signal");
        sync_receiver.map(|i| {
            i
        }).wait().unwrap()
    }
}
