//! Release staging is the same preserving merge as update apply. No mkfs or
//! unconditional configuration removal belongs to an ordinary system update.
use crate::{error::refuse, target::Target};
use anyhow::{Context, Result, ensure};
use std::os::unix::fs::FileTypeExt;
use std::{
    fs,
    path::{Path, PathBuf},
    process::Command,
};
/// The prebuilt system `stage` writes: files, or a release medium holding them.
#[derive(Debug, Clone)]
pub enum StageSource {
    Files(StageRequest),
    /// A release medium (its device, or where it is mounted): the rootfs,
    /// kernel and installed initramfs at the live protocol's paths.
    Media(PathBuf),
    /// Unsigned local media explicitly trusted by the caller.
    LocalMedia(PathBuf),
    /// One typed release bundle (including an imported cache object).
    Manifest {
        path: PathBuf,
        trusted_local: bool,
    },
}

/// The prebuilt system `stage` writes, as files.
#[derive(Debug, Clone)]
pub struct StageRequest {
    /// EROFS rootfs file, bound to its corresponding release manifest.
    pub rootfs: PathBuf,
    pub kernel: PathBuf,
    pub initrd: Option<PathBuf>,
}

pub(crate) fn merge(source: &StageSource, change_flavour: bool) -> Result<()> {
    crate::filesystem::private_mounts()?;
    let target = Target::running()?;
    let (_medium, manifest, local, files) = match source {
        StageSource::Manifest {
            path,
            trusted_local,
        } => (None, path.clone(), *trusted_local, None),
        StageSource::Media(media) | StageSource::LocalMedia(media) => {
            let (mount, root) = open_media(&target, media)?;
            (
                mount,
                root.join(distro_contract::release::MANIFEST),
                matches!(source, StageSource::LocalMedia(_)),
                None,
            )
        }
        StageSource::Files(files) => {
            let parent = files
                .rootfs
                .parent()
                .context("raw stage source needs a release manifest")?;
            let adjacent = parent.join(distro_contract::release::MANIFEST);
            let manifest = if adjacent.exists() {
                adjacent
            } else {
                parent
                    .parent()
                    .context("raw stage source needs a corresponding release manifest")?
                    .join(distro_contract::release::MANIFEST)
            };
            ensure!(
                manifest.is_file(),
                "raw stage inputs require their corresponding release.json; use --manifest for explicit local trust"
            );
            (None, manifest, false, Some(files))
        }
    };
    if let Some(files) = files {
        let verified = crate::release::for_target(&target, &manifest, local, change_flavour)?;
        for (path, artifact) in [
            (&files.rootfs, &verified.manifest().artifacts.rootfs),
            (&files.kernel, &verified.manifest().artifacts.kernel),
            (
                files
                    .initrd
                    .as_ref()
                    .context("raw stage requires matching installed initramfs")?,
                &verified.manifest().artifacts.initramfs,
            ),
        ] {
            let mut f = levitate_recipe::fsx::open_regular(path)?;
            ensure!(
                f.metadata()?.len() == artifact.size
                    && crate::generation::hash_reader(&mut f)? == artifact.sha256,
                "raw stage files differ from their release manifest"
            );
        }
    }
    let status =
        crate::update::apply_options(None, Some(&manifest), local, None, None, change_flavour)?;
    ensure!(
        status.merge_complete,
        "release has conflicts or requires recovery; inspect recab update conflicts"
    );
    crate::finalize(&crate::FinalizeRequest {
        change_flavour,
        ..Default::default()
    })?;
    Ok(())
}
pub(crate) fn open_media(target: &Target, media: &Path) -> Result<(Option<Mounted>, PathBuf)> {
    let meta = fs::metadata(media)
        .map_err(|err| refuse(format!("release medium '{}': {err}", media.display())))?;
    let (mounted, root) = if meta.is_dir() {
        (None, media.to_path_buf())
    } else if meta.file_type().is_block_device() {
        let point = target.stage_mount("media");
        fs::create_dir_all(&point).with_context(|| format!("creating '{}'", point.display()))?;
        (
            Some(Mounted::new(target, &["-o", "ro"], media, &point)?),
            point,
        )
    } else {
        return Err(refuse(format!(
            "release medium '{}' is neither a block device nor a directory",
            media.display()
        )));
    };
    Ok((mounted, root))
}

/// A filesystem mounted for the stage, unmounted when dropped.
pub(crate) struct Mounted {
    point: PathBuf,
    umount: String,
    active: bool,
}

impl Mounted {
    /// `source` mounted at `point` with `mount`'s `options`.
    pub(crate) fn new(
        target: &Target,
        options: &[&str],
        source: &Path,
        point: &Path,
    ) -> Result<Self> {
        target_root::run(
            Command::new(&target.programs.mount)
                .args(options)
                .arg(source)
                .arg(point),
        )?;
        Ok(Self {
            point: point.to_path_buf(),
            umount: target.programs.umount.clone(),
            active: true,
        })
    }

    pub(crate) fn unmount(mut self) -> Result<()> {
        target_root::run(Command::new(&self.umount).arg(&self.point))?;
        self.active = false;
        Ok(())
    }
}

impl Drop for Mounted {
    fn drop(&mut self) {
        if self.active {
            if let Err(err) = target_root::run(Command::new(&self.umount).arg(&self.point)) {
                eprintln!("recab: warning: {err:#}");
            }
        }
    }
}
