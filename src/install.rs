//! Installation / suppression des noyaux externes (CachyOS, Zen, Liquorix)
//! sur Solus.
//!
//! Étapes d'installation d'un paquet `.pkg.tar.zst` (identiques pour les
//! trois noyaux, ils sont tous des paquets pacman « standard ») :
//!   1. `sudo tar` (`-I zstd`) pour extraire `usr/lib/modules/<ver>` →
//!      `/lib/modules/<ver>` (merged-usr : extrait directement sous `/`).
//!   2. `depmod <ver>` : le paquet ne contient PAS `modules.dep` /
//!      `modules.alias` (produits par le post-install pacman, qu'on ne joue
//!      pas) ; sans eux `dracut` refuse de démarrer. Étape bloquante.
//!   3. Extraction + copie du `vmlinuz` du paquet vers `/boot/EFI/<vendor>/kernel-<ver>`.
//!      Le chemin du vmlinuz DANS le paquet dépend du noyau :
//!        - CachyOS  : `usr/lib/modules/<ver>/vmlinuz`
//!        - Zen / Liquorix : `boot/vmlinuz-<ver>`
//!   4. `dracut --kver <ver>` pour générer l'initrd
//!      → `/boot/EFI/<vendor>/initrd-<ver>`.
//!   5. Écriture de l'entrée systemd-boot `/boot/loader/entries/ext-<ver>.conf`
//!      (mêmes `options` que l'entrée Solus — root PARTUUID, console, …).
//!   6. `sync`.
//!
//! La partition /boot est montée via `sudo clr-boot-manager mount-boot`
//! (idempotent si déjà montée).

use std::fs;
use std::path::Path;

use crate::repo::{KernelKind, PkgLayout, discover_pkg_layout};
use crate::sysutil::{KernelPaths, log, run_sudo};

/// Ouvre (en montant au besoin) la partition /boot.
pub fn ensure_boot_mounted(password: &str) -> Result<(), String> {
    // Déjà montée ?
    let mounted = fs::read_to_string("/proc/mounts")
        .map(|m| m.lines().any(|l| l.split_whitespace().nth(1) == Some("/boot")))
        .unwrap_or(false);
    if !mounted {
        log("Montage de /boot via clr-boot-manager …");
        let (code, _out, err) = run_sudo(password, "clr-boot-manager", &["mount-boot"]);
        if !mounted_after() {
            return Err(format!("Impossible de monter /boot (code {}): {}", code, err.trim()));
        }
    }
    Ok(())
}

fn mounted_after() -> bool {
    fs::read_to_string("/proc/mounts")
        .map(|m| m.lines().any(|l| l.split_whitespace().nth(1) == Some("/boot")))
        .unwrap_or(false)
}

/// Installe le paquet (déjà téléchargé et vérifié) du noyau `kernel_version`.
///
/// La structure interne du paquet (dossier modules + vmlinuz) est
/// **découverte** depuis l'archive plutôt que déduite du nom : le nom du
/// dossier `usr/lib/modules/<NAME>` fait foi d'identité du noyau et le
/// `vmlinuz` peut vivre dans `usr/lib/modules/<NAME>/vmlinuz` (CachyOS,
/// Liquorix) ou `boot/vmlinuz-<NAME>` (Zen).
///
/// `kind` sert au titre de boot. `step` est appelé avec (étape, texte) à chaque
/// phase — le thread UI doit le rediriger vers la barre de progression.
pub fn install_kernel(
    password: &str,
    pkg_path: &Path,
    kernel_version: &str,
    kind: KernelKind,
    step: &dyn Fn(u32, &str),
) -> Result<String, String> {
    // 0. Découverte de la structure interne (dossier modules + vmlinuz).
    step(0, "Analyse de la structure du paquet …");
    let layout: PkgLayout = match discover_pkg_layout(pkg_path) {
        Ok(l) => l,
        Err(e) => return Err(format!("Structure du paquet illisible : {}", e)),
    };
    // `k` = nom du dossier modules = identité du noyau (uname -r).
    let k = &layout.mod_dir;
    let kp = KernelPaths::for_kernel(k, kind);

    // 1. /boot monté ?
    step(1, "Vérification du montage de /boot …");
    ensure_boot_mounted(password)?;

    // 2. Extraction des modules dans /lib/modules/<k> (merged-usr).
    //
    // IMPORTANT : le membre dans le paquet s'appelle `usr/lib/modules/<k>`,
    // il faut donc extraire avec `-C /` (racine) et NON avec `-C /usr`.
    step(2, &format!("Extraction des modules /lib/modules/{} …", k));
    let member = format!("usr/lib/modules/{}", k);
    let (code, _out, err) = run_sudo(
        password,
        "tar",
        &[
            "-x",
            "-I",
            "zstd",
            "-f",
            pkg_path.to_str().ok_or("chemin invalide")?,
            "-C",
            "/",
            &member,
        ],
    );
    if code != 0 {
        return Err(format!(
            "Échec de l'extraction des modules (code {}): {}",
            code,
            err.lines().next().unwrap_or("")
        ));
    }
    // Garde : les modules DOIVENT exister au bon endroit avant de lancer dracut.
    if !kp.modules.exists() {
        return Err(format!(
            "les modules n'ont pas été extraits dans /lib/modules/{} (le noyau n'est pas au bon endroit). \
             Vérifiez le montage et la structure /lib -> usr/lib de la distribution, puis réessayez.",
            k
        ));
    }

    // 3. Tables de modules (modules.dep, modules.alias, …).
    // INDISPENSABLE avant dracut : les paquets n'en fournissent pas — ils sont
    // générés par le `depmod` du post-install pacman, qu'on n'exécute pas.
    step(3, &format!("depmod {} …", k));
    let (code, _out, err) = run_sudo(password, "depmod", &[k]);
    if code != 0 {
        return Err(format!(
            "depmod a échoué pour {} (code {}): {}",
            k,
            code,
            tail(&err)
        ));
    }
    let dep_path = kp.modules.join("modules.dep");
    if !dep_path.exists() {
        return Err(format!(
            "depmod n'a pas généré {} : l'initrd ne peut pas être construit",
            dep_path.display()
        ));
    }

    // 4. vmlinuz → /boot/EFI/<vendor>/
    // Le vmlinuz est dans le paquet ; son membre est découvert (`layout.vmlinuz`).
    step(4, "Extraction + copie du vmlinuz dans /boot …");
    let tmp = crate::sysutil::state_dir().join(format!("vmlinuz-{}", k));
    let _ = fs::create_dir_all(&tmp);
    let (code, _out, err) = run_sudo(
        password,
        "tar",
        &[
            "-x",
            "-I",
            "zstd",
            "-f",
            pkg_path.to_str().ok_or("chemin invalide")?,
            "-C",
            tmp.to_str().ok_or("chemin invalide")?,
            &layout.vmlinuz,
        ],
    );
    if code != 0 {
        let _ = fs::remove_dir_all(&tmp);
        return Err(format!(
            "Échec de l'extraction de vmlinuz (code {}): {}",
            code,
            err.lines().next().unwrap_or("")
        ));
    }
    let src_vmlinuz = tmp.join(&layout.vmlinuz);
    if !src_vmlinuz.exists() {
        let _ = fs::remove_dir_all(&tmp);
        return Err(format!(
            "vmlinuz introuvable après extraction (attendu : {})",
            src_vmlinuz.display()
        ));
    }
    let boot_dir = kp.vmlinuz.parent().unwrap().to_path_buf();
    let (code, _out, err) = run_sudo(password, "mkdir", &["-p", boot_dir.to_str().ok_or("x")?]);
    if code != 0 {
        return Err(format!("mkdir {} : {}", boot_dir.display(), err.trim()));
    }
    let (code, _out, err) = run_sudo(
        password,
        "install",
        &["-m", "0755", src_vmlinuz.to_str().ok_or("x")?, kp.vmlinuz.to_str().ok_or("x")?],
    );
    if code != 0 {
        let _ = fs::remove_dir_all(&tmp);
        return Err(format!("Copie du vmlinuz impossible (code {}): {}", code, err.trim()));
    }

    // 5. Initrd avec dracut (k = nom du dossier modules).
    let dp = std::path::Path::new("/usr/bin/dracut");
    if !dp.exists() {
        let _ = fs::remove_file(&kp.vmlinuz);
        let _ = fs::remove_dir_all(&tmp);
        return Err("le paquet `dracut` est requis pour générer l'initrd, mais il n'est pas installé. \
                    Sur Solus : `sudo eopkg install dracut`, puis réessayez.".to_string());
    }
    let dracut_msg = format!("Génération de l'initrd (dracut {}), c'est long …", k);
    step(5, &dracut_msg);
    let (code, _out, err) = run_sudo(
        password,
        "dracut",
        &["--force", "--kver", k, kp.initrd.to_str().ok_or("x")?],
    );
    if code != 0 {
        let _ = fs::remove_file(&kp.vmlinuz);
        let _ = fs::remove_dir_all(&tmp);
        return Err(format!("dracut a échoué (code {}): {}", code, tail(&err)));
    }
    if !kp.initrd.exists() {
        let _ = fs::remove_file(&kp.vmlinuz);
        let _ = fs::remove_dir_all(&tmp);
        return Err("l'initrd n'a pas été créée par dracut".into());
    }

    // 6. Entrée systemd-boot.
    step(6, "Création de l'entrée de boot …");
    let options = solus_boot_options()
        .unwrap_or_else(|| "root=PARTUUID= rootflags=subvol=@ rw".into());
    // Les chemins dans l'entrée de boot sont relatifs à la racine ESP :
    // /boot/EFI/… → /EFI/…
    let linux_rel = kp.vmlinuz.to_string_lossy().replace("/boot/EFI/", "/EFI/");
    let initrd_rel = kp.initrd.to_string_lossy().replace("/boot/EFI/", "/EFI/");
    let entry = format!(
        "title {} {} (Solus)\nlinux {}\ninitrd {}\noptions {}\n",
        kind.boot_title(),
        kernel_version,
        linux_rel,
        initrd_rel,
        options
    );
    // Écrire via sudo tee.
    // `printf '%b'` interprète les `\n` échappés (escape_shell les produit).
    let (code, _out, err) = run_sudo(
        password,
        "sh",
        &["-c", &format!("printf '%b' '{}' | tee {} >/dev/null", escape_shell(&entry), kp.boot_entry.to_str().ok_or("x")?)],
    );
    if code != 0 {
        return Err(format!("Écriture de l'entrée boot impossible : {}", err.trim()));
    }

    // 7. sync.
    step(7, "sync …");
    let (code, _out, _err) = run_sudo(password, "sync", &[]);
    if code != 0 {
        return Err("sync a échoué".into());
    }

    // Nettoyage du tmp.
    let _ = fs::remove_dir_all(&tmp);

    step(8, "Terminé ✓");
    log(&format!("Noyau {} ({}) installé ✓", kernel_version, kind.label()));
    Ok(kp.boot_entry.to_string_lossy().to_string())
}

/// Récupère la ligne `options` d'une entrée Solus existante (pour réutiliser
/// root PARTUUID, keymap, splash, …).
fn solus_boot_options() -> Option<String> {
    let dir = "/boot/loader/entries";
    for e in fs::read_dir(dir).ok()?.flatten() {
        let name = e.file_name().to_string_lossy().to_string();
        if !name.starts_with("Solus") {
            continue;
        }
        if let Ok(content) = fs::read_to_string(e.path()) {
            for line in content.lines() {
                if let Some(rest) = line.strip_prefix("options ") {
                    return Some(rest.trim().to_string());
                }
            }
        }
    }
    None
}

fn tail(s: &str) -> String {
    let all: Vec<&str> = s.lines().collect();
    let start = all.len().saturating_sub(3);
    all[start..].join(" | ")
}

fn escape_shell(s: &str) -> String {
    s.replace('\\', "\\\\").replace('\'', "'\\''").replace('\n', "\\n")
}

/// Supprime un noyau externe non courant (fichiers + entrée boot).
pub fn remove_kernel(password: &str, kernel_version: &str) -> Result<(), String> {
    if kernel_version == crate::sysutil::running_kernel() {
        return Err("Impossible : c'est le noyau en cours d'exécution.".into());
    }
    ensure_boot_mounted(password)?;
    let kp = KernelPaths::for_version(kernel_version);

    for f in [&kp.vmlinuz, &kp.initrd, &kp.boot_entry] {
        if f.exists() {
            let (code, _o, err) = run_sudo(password, "rm", &["-f", f.to_str().ok_or("x")?]);
            if code != 0 {
                return Err(format!("Suppression de {} : {}", f.display(), err.trim()));
            }
        }
    }
    let (code, _o, err) = run_sudo(password, "rm", &["-rf", kp.modules.to_str().ok_or("x")?]);
    if code != 0 {
        return Err(format!("Suppression /lib/modules/{} : {}", kernel_version, err.trim()));
    }
    run_sudo(password, "sync", &[]);
    log(&format!("Noyau {} supprimé ✓", kernel_version));
    Ok(())
}

/// Un noyau externe installé + celui en cours d'exécution.
pub struct InstalledKernel {
    pub version: String,
    pub running: bool,
    /// Type de noyau (CachyOS / Zen / Liquorix), déduit de la version.
    pub kind: KernelKind,
    #[allow(dead_code)]
    pub path: KernelPaths,
}

/// Liste des noyaux externes installés (tous types) + celui en cours.
pub fn list_installed() -> Vec<InstalledKernel> {
    let running = crate::sysutil::running_kernel();
    crate::sysutil::installed_ext_kernels()
        .into_iter()
        .map(|v| InstalledKernel {
            kind: kind_from_version(&v),
            version: v.clone(),
            running: v == running,
            path: KernelPaths::for_version(&v),
        })
        .collect()
}

/// Liste des noyaux installés d'un type donné (par version-échantillon).
#[allow(dead_code)]
pub fn list_installed_kind(sample: &str) -> Vec<InstalledKernel> {
    let running = crate::sysutil::running_kernel();
    crate::sysutil::installed_ext_kernels_suffix(sample)
        .into_iter()
        .map(|v| InstalledKernel {
            kind: kind_from_version(&v),
            version: v.clone(),
            running: v == running,
            path: KernelPaths::for_version(&v),
        })
        .collect()
}

/// Déduit le [`KernelKind`] d'une version de noyau installée.
pub fn kind_from_version(v: &str) -> KernelKind {
    match crate::sysutil::kernel_suffix(v).as_str() {
        s if s.contains("cachyos") => KernelKind::Cachyos,
        s if s.contains("zen") => KernelKind::Zen,
        s if s.contains("lqx") => KernelKind::Liquorix,
        _ => KernelKind::Cachyos,
    }
}
