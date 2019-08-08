use cgroups::{self, cgroup_builder::CgroupBuilder};
use std::path::PathBuf;
use nix::unistd::{self, Pid, ForkResult};
use vmm::vmm_config::boot_source::BootSourceConfig;
use vmm::vmm_config::drive::BlockDeviceConfig;
use vmm::vmm_config::vsock::VsockDeviceConfig;

use crate::vmm_wrapper::VmmWrapper;

pub struct VmAppConfig {
    pub instance_id: String,
    pub vsock_cid: u32,
    pub kernel: String,
    pub rootfs: PathBuf,
    pub appfs: Option<PathBuf>,
    pub cmd_line: String,
    pub seccomp_level: u32,
    pub cpu_share: u64,
}

pub struct VmApp {
    pub config: VmAppConfig,
    pub process: Pid,
}

impl VmApp {
    pub fn kill(&mut self) {
        nix::sys::signal::kill(self.process, nix::sys::signal::Signal::SIGKILL).expect("Failed to kill child");
        self.wait();
    }

    pub fn wait(&mut self) {
        nix::sys::wait::waitpid(self.process, None).expect("Failed to kill child");
    }
}

impl Drop for VmApp {
    fn drop(&mut self) {
        self.kill();
    }
}

impl VmAppConfig {
    pub fn run(self) -> VmApp {
        match unistd::fork() {
            Err(_) => panic!("Couldn't fork!!"),
            Ok(ForkResult::Parent { child, .. }) => {
                let pid = child.as_raw() as u64;
                let v1 = cgroups::hierarchies::V1::new();
                let cgroup_name = self.instance_id.clone();
                CgroupBuilder::new(cgroup_name.as_str(), &v1)
                    .cpu()
                        .shares(self.cpu_share)
                        .done()
                    .build().add_task(pid.into()).expect("Adding child to Cgroup");
                return VmApp {
                    config: self,
                    process: child,
                }
            },
            Ok(ForkResult::Child) => {
                let mut vmm = VmmWrapper::new(self.instance_id.clone(), self.seccomp_level);

                let boot_config = BootSourceConfig {
                    kernel_image_path: self.kernel,
                    boot_args: Some(self.cmd_line),
                };
                vmm.set_boot_source(boot_config).expect("bootsource");

                let block_config = BlockDeviceConfig {
                    drive_id: String::from("rootfs"),
                    path_on_host: self.rootfs,
                    is_root_device: true,
                    is_read_only: true,
                    partuuid: None,
                    rate_limiter: None,
                };
                vmm.insert_block_device(block_config).expect("Rootfs");
                if let Some(appfs) = self.appfs {
                    let block_config = BlockDeviceConfig {
                        drive_id: String::from("appfs"),
                        path_on_host: appfs,
                        is_root_device: false,
                        is_read_only: true,
                        partuuid: None,
                        rate_limiter: None,
                    };
                    vmm.insert_block_device(block_config).expect("AppBlk");
                }

                vmm.add_vsock(VsockDeviceConfig { id: self.instance_id, guest_cid: self.vsock_cid }).expect("vsock");

                vmm.start_instance().expect("Start");
                vmm.join();
                std::process::exit(0);
            }
        }
    }
}
