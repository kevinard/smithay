use drm::{Device as BasicDevice, control::Device as ControlDevice};
use std::{
    os::unix::io::{AsFd, AsRawFd, BorrowedFd, RawFd},
    sync::{
        Arc, Weak,
        atomic::{AtomicBool, Ordering},
    },
};
use tracing::{error, info, warn};

use crate::backend::drm::{DrmNode, NodeType};
use crate::utils::{DevPath, DeviceFd};

#[derive(Debug)]
struct InternalDrmDeviceFd {
    fd: DeviceFd,
    privileged: bool,
    keep_last_frame: AtomicBool,
}

impl PartialEq for InternalDrmDeviceFd {
    fn eq(&self, other: &Self) -> bool {
        self.fd == other.fd
    }
}

impl Drop for InternalDrmDeviceFd {
    fn drop(&mut self) {
        info!("Dropping device: {:?}", self.fd.dev_path());
        if self.privileged {
            if let Err(err) = self.release_master_lock() {
                error!("Failed to drop drm master state. Error: {}", err);
            }
        }
    }
}

impl AsFd for InternalDrmDeviceFd {
    fn as_fd(&self) -> BorrowedFd<'_> {
        self.fd.as_fd()
    }
}
impl BasicDevice for InternalDrmDeviceFd {}
impl ControlDevice for InternalDrmDeviceFd {}

/// Ref-counted file descriptor of an open drm device
#[derive(Debug, Clone, PartialEq)]
pub struct DrmDeviceFd(Arc<InternalDrmDeviceFd>);

impl AsFd for DrmDeviceFd {
    fn as_fd(&self) -> BorrowedFd<'_> {
        self.0.fd.as_fd()
    }
}

// TODO: drop impl once not needed anymore by smithay or dependencies
impl AsRawFd for DrmDeviceFd {
    fn as_raw_fd(&self) -> RawFd {
        self.0.fd.as_raw_fd()
    }
}

impl DrmDeviceFd {
    /// Create a new `DrmDeviceFd`.
    ///
    /// This function will try to acquire the master lock for the underlying drm device
    /// and release the lock on drop again.
    /// For that reason you should never create multiple `DrmDeviceFd` out of the same
    /// `DeviceFd`, but instead clone the `DrmDeviceFd`.
    ///
    /// Failing to do so might fail to acquire set lock and release it early,
    /// which can cause some drm ioctls to fail later.
    pub fn new(fd: DeviceFd) -> DrmDeviceFd {
        let mut dev = InternalDrmDeviceFd {
            fd,
            privileged: false,
            keep_last_frame: AtomicBool::new(false),
        };

        // Render nodes have no master; the kernel refuses SET_MASTER on them.
        let is_render_node = DrmNode::from_file(&dev.fd).is_ok_and(|node| node.ty() == NodeType::Render);

        // We want to modeset, so we better be the master, if we run via a tty session.
        // This is only needed on older kernels. Newer kernels grant this permission,
        // if no other process is already the *master*. So we skip over this error.
        if !is_render_node {
            if dev.acquire_master_lock().is_err() {
                warn!("Unable to become drm master, assuming unprivileged mode");
            } else {
                dev.privileged = true;
            }
        }

        DrmDeviceFd(Arc::new(dev))
    }

    pub(in crate::backend::drm) fn is_privileged(&self) -> bool {
        self.0.privileged
    }

    /// Returns the underlying `DeviceFd`
    pub fn device_fd(&self) -> DeviceFd {
        self.0.fd.clone()
    }

    /// Returns the `dev_t` of the underlying device
    pub fn dev_id(&self) -> rustix::io::Result<libc::dev_t> {
        Ok(rustix::fs::fstat(&self.0.fd)?.st_rdev)
    }

    /// Leave the current frame on screen when the device is dropped.
    ///
    /// By default, dropping a [`DrmDevice`](super::DrmDevice) and its surfaces
    /// disables the outputs it drove and restores the state the device had when it
    /// was opened, and dropping a framebuffer that is still being scanned out turns
    /// its plane off. With this set, surfaces and the device leave the hardware as
    /// it is, and framebuffers are closed with `DRM_IOCTL_MODE_CLOSEFB` (Linux 6.9),
    /// which keeps a framebuffer on its plane until the next DRM master replaces it.
    /// A compositor that hands the display to another one can set this right before
    /// shutting down, so that the next one can take over without a blank screen and,
    /// if it keeps the same modes, without a modeset.
    ///
    /// On kernels without `CLOSEFB`, framebuffers are removed as usual and their
    /// planes turn off. Whatever the kernel does once the last file descriptor
    /// is closed (such as restoring the fbdev console) still applies.
    pub fn set_keep_last_frame(&self, keep: bool) {
        self.0.keep_last_frame.store(keep, Ordering::SeqCst);
    }

    /// Whether [`DrmDeviceFd::set_keep_last_frame`] is in effect.
    pub fn keeps_last_frame(&self) -> bool {
        self.0.keep_last_frame.load(Ordering::SeqCst)
    }

    /// Remove a framebuffer handle, keeping it on screen when the last frame
    /// is kept and the kernel supports `CLOSEFB`.
    pub(in crate::backend::drm) fn release_framebuffer(
        &self,
        fb: drm::control::framebuffer::Handle,
    ) -> std::io::Result<()> {
        if self.keeps_last_frame() {
            match close_framebuffer(self.as_fd(), fb.into()) {
                Ok(()) => return Ok(()),
                // Older kernels: fall back to RMFB, which turns the plane off.
                Err(err) if matches!(err.raw_os_error(), Some(libc::EINVAL | libc::ENOTTY)) => {}
                Err(err) => return Err(err),
            }
        }
        self.destroy_framebuffer(fb)
    }

    /// Returns a weak reference to the underlying device
    pub fn downgrade(&self) -> WeakDrmDeviceFd {
        WeakDrmDeviceFd(Arc::downgrade(&self.0))
    }
}

impl BasicDevice for DrmDeviceFd {}
impl ControlDevice for DrmDeviceFd {}

/// Weak variant of [`DrmDeviceFd`]
#[derive(Debug, Clone, Default)]
pub struct WeakDrmDeviceFd(Weak<InternalDrmDeviceFd>);

impl WeakDrmDeviceFd {
    /// Construct an empty Weak reference, that will never upgrade successfully
    pub fn new() -> Self {
        WeakDrmDeviceFd(Weak::new())
    }

    /// Try to upgrade to a strong reference
    pub fn upgrade(&self) -> Option<DrmDeviceFd> {
        self.0.upgrade().map(DrmDeviceFd)
    }
}

impl PartialEq for WeakDrmDeviceFd {
    fn eq(&self, other: &Self) -> bool {
        Weak::ptr_eq(&self.0, &other.0)
    }
}

impl PartialEq<DrmDeviceFd> for WeakDrmDeviceFd {
    fn eq(&self, other: &DrmDeviceFd) -> bool {
        Weak::upgrade(&self.0).is_some_and(|arc| Arc::ptr_eq(&arc, &other.0))
    }
}

/// `DRM_IOCTL_MODE_CLOSEFB`: `DRM_IOWR(0xD0, struct drm_mode_closefb)`.
const DRM_IOCTL_MODE_CLOSEFB: libc::c_ulong = (3 << 30)
    | ((std::mem::size_of::<drm_ffi::drm_mode_closefb>() as libc::c_ulong) << 16)
    | ((b'd' as libc::c_ulong) << 8)
    | 0xD0;

fn close_framebuffer(fd: BorrowedFd<'_>, fb_id: u32) -> std::io::Result<()> {
    let mut request = drm_ffi::drm_mode_closefb { fb_id, pad: 0 };
    // SAFETY: the request is a valid, initialised drm_mode_closefb that
    // outlives the call, matching the ioctl's argument type.
    let result = unsafe {
        libc::ioctl(
            fd.as_raw_fd(),
            DRM_IOCTL_MODE_CLOSEFB as _,
            &mut request as *mut drm_ffi::drm_mode_closefb,
        )
    };
    if result == 0 {
        Ok(())
    } else {
        Err(std::io::Error::last_os_error())
    }
}

#[cfg(test)]
mod tests {
    #[test]
    fn closefb_request_number_matches_the_kernel() {
        // include/uapi/drm/drm.h: DRM_IOWR(0xD0, struct drm_mode_closefb)
        assert_eq!(super::DRM_IOCTL_MODE_CLOSEFB, 0xC008_64D0);
    }
}
