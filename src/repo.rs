//! Interrogation des dépôts pacman et téléchargement des paquets noyau.
//!
//! Trois sources « pacman » distinctes, toutes avec la même structure
//! d'enregistrement `%TAG%\nvalue` dans leur base `.db.tar.(zst|xz)` :
//!
//!  - **CachyOS**   `https://mirror.cachyos.org/repo/{arch}/…`
//!  - **Zen**       `https://<mirror-arch>/archlinux/{repo}/os/x86_64`
//!  - **Liquorix**  `https://liquorix.net/archlinux/{repo}/x86_64`
//!
//! La base (format pacman-db) contient un enregistrement par paquet, dont
//! `%NAME%`, `%VERSION%`, `%SHA256SUM%`, `%CSIZE%`, `%FILENAME%` (le nom exact
//! du `.pkg.tar.zst`).
//!
//! Chaque noyau est décrit par sa [`KernelKind`]. La fonction
//! [`fetch_latest_kernels`] interroge la base, trie du plus récent au plus
//! ancien et renvoie les 3 dernières lignes.

use std::io::Read;
use std::path::PathBuf;
use std::process::Command;
use std::time::Duration;

use tar::Archive;
use ureq::Agent;
use xz2::read::XzDecoder;
use flate2::read::GzDecoder;

/// Type de noyau géré par l'application.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum KernelKind {
    /// Noyau CachyOS (`linux-cachyos`).
    Cachyos,
    /// Noyau Zen d'Arch Linux (`linux-zen`).
    Zen,
    /// Noyau Liquorix (`linux-lqx`).
    Liquorix,
}

impl KernelKind {
    /// Libellé court descriptif pour l'UI.
    pub fn label(self) -> &'static str {
        match self {
            KernelKind::Cachyos => "CachyOS",
            KernelKind::Zen => "Zen",
            KernelKind::Liquorix => "Liquorix",
        }
    }
    /// Nom du paquet principal dans le dépôt.
    pub fn pkg_name(self) -> &'static str {
        match self {
            KernelKind::Cachyos => "linux-cachyos",
            KernelKind::Zen => "linux-zen",
            KernelKind::Liquorix => "linux-lqx",
        }
    }
    /// Sous-dépôt EFI « propriétaire » (pour isoler de `clr-boot-manager`).
    pub fn vendor_dir(self) -> &'static str {
        match self {
            KernelKind::Cachyos => "com.cachyos",
            KernelKind::Zen => "com.zen-kernel",
            KernelKind::Liquorix => "com.liquorix",
        }
    }
    /// Titre de l'entrée de boot (systemd-boot).
    pub fn boot_title(self) -> &'static str {
        match self {
            KernelKind::Cachyos => "CachyOS",
            KernelKind::Zen => "Zen",
            KernelKind::Liquorix => "Liquorix",
        }
    }
}

/// Format de compression d'une base pacman-db.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DbFormat {
    /// Zstandard — magic `28 b5 2f fd`.
    Zstd,
    /// LZMA (xz) — magic `fd 37 7a 58 5a 00`.
    Xz,
    /// Gzip — magic `1f 8b`.
    Gzip,
    /// Format inconnu ou page HTML.
    Unknown,
}

/// Détecte le format de compression d'une base pacman-db à partir de
/// ses premiers octets.
pub fn detect_db_format(bytes: &[u8]) -> DbFormat {
    if bytes.len() >= 4 && &bytes[0..4] == b"\x28\xb5\x2f\xfd" {
        DbFormat::Zstd
    } else if bytes.len() >= 6 && &bytes[0..6] == b"\xfd7zXZ\x00" {
        DbFormat::Xz
    } else if bytes.len() >= 2 && bytes[0] == 0x1f && bytes[1] == 0x8b {
        DbFormat::Gzip
    } else {
        DbFormat::Unknown
    }
}

// ------------------------------------------------------------------
// Découverte de la structure interne d'un paquet
// ------------------------------------------------------------------

/// Structure interne découverte dynamiquement dans un paquet `.pkg.tar.zst`.
///
/// Le nom du répertoire `usr/lib/modules/<NAME>` fait foi d'identité du noyau
/// (l'`uname -r` du noyau installé est exactement `<NAME>`) ; il ne se déduit
/// pas du nom de paquet :
///   - Liquorix `linux-lqx-7.2.12.lqx1-1`  → modules `7.2.12-lqx1-1-lqx`
///   - Zen      `linux-zen-7.2.11.zen1-1`  → modules `7.2.11.zen1`
///   - CachyOS  `linux-cachyos-7.3.0-4`    → modules `7.3.0-4`
/// Le `vmlinuz` est dans `usr/lib/modules/<NAME>/vmlinuz` pour CachyOS et
/// Liquorix, et dans `boot/vmlinuz-<NAME>` pour Zen — on ne sait jamais à
/// l'avance, donc on le **découvre** plutôt que de le deviner.
#[derive(Debug, Clone)]
pub struct PkgLayout {
    /// Nom du répertoire `usr/lib/modules/<NAME>` (= `uname -r` du noyau).
    pub mod_dir: String,
    /// Chemin relatif du `vmlinuz` dans le paquet.
    pub vmlinuz: String,
    /// Chemin relatif de l'initramfs dans le paquet (si fournie ; sinon dracut).
    #[allow(dead_code)]
    pub initramfs: Option<String>,
}

/// Découvre la structure interne d'un paquet pacman `.pkg.tar.zst` en listant
/// ses membres avec `tar -tI zstd` (commande système, sans dépendance Rust).
///
/// Retourne le nom du dossier `usr/lib/modules/<NAME>`, le chemin du
/// `vmlinuz` et, si présent, celui de l'initramfs fournie par le paquet.
pub fn discover_pkg_layout(pkg: &std::path::Path) -> Result<PkgLayout, String> {
    let out = std::process::Command::new("tar")
        .args(["-tI", "zstd", "-f"])
        .arg(pkg)
        .output()
        .map_err(|e| format!("impossible de lister le paquet : {}", e))?;
    if !out.status.success() {
        return Err(format!(
            "tar -t a échoué (code {}): {}",
            out.status.code().unwrap_or(-1),
            String::from_utf8_lossy(&out.stderr).lines().next().unwrap_or("")
        ));
    }
    let listing = String::from_utf8_lossy(&out.stdout).to_string();

    let mut mod_dir: Option<String> = None;
    let mut vmlinuz: Option<String> = None;
    let mut initramfs: Option<String> = None;
    for line in listing.lines() {
        let t = line.trim_end_matches('/');
        if mod_dir.is_none() {
            if let Some(rest) = t.strip_prefix("usr/lib/modules/") {
                if rest
                    .chars()
                    .all(|c| c.is_ascii_alphanumeric() || c == '.' || c == '-')
                {
                    mod_dir = Some(rest.to_string());
                }
            }
        }
        if vmlinuz.is_none() {
            if t == "boot/vmlinuz" || t.starts_with("boot/vmlinuz-") {
                vmlinuz = Some(t.to_string());
            } else if let Some(rest) = t.strip_prefix("usr/lib/modules/") {
                if rest.ends_with("/vmlinuz") {
                    vmlinuz = Some(t.to_string());
                }
            }
        }
        if initramfs.is_none() && t.starts_with("boot/initramfs-") {
            initramfs = Some(t.to_string());
        }
        if mod_dir.is_some() && vmlinuz.is_some() && initramfs.is_some() {
            break;
        }
    }
    let mod_dir = mod_dir.ok_or("aucun dossier usr/lib/modules/<ver> dans le paquet")?;
    let vmlinuz = vmlinuz
        .or_else(|| Some(format!("usr/lib/modules/{}/vmlinuz", mod_dir)))
        .ok_or("vmlinuz introuvable")?;
    Ok(PkgLayout { mod_dir, vmlinuz, initramfs })
}

/// Un noyau disponible dans un dépôt.
///
/// NOTE : la structure interne réelle du paquet (dossier modules, chemin du
/// vmlinuz) n'est PAS déduite ici — elle est **découverte** depuis l'archive
/// téléchargée par [`discover_pkg_layout`]. Ce tableau ne porte que le
/// libellé d'affichage de la version.
#[derive(Debug, Clone)]
pub struct KernelRelease {
    /// Type de noyau (CachyOS / Zen / Liquorix).
    pub kind: KernelKind,
    /// Nom du paquet (ex. `linux-cachyos`).
    pub pkg_name: String,
    /// Version pacman (ex. `7.3.0-4`, `7.2.11.zen1-1`, `7.2.12.lqx1-1`).
    pub version: String,
    /// Libellé d'affichage de la version du noyau (non normatif — le nom du
    /// dossier `usr/lib/modules/<…>` du paquet fait foi, voir `PkgLayout`).
    pub kernel_version: String,
    /// SHA256 du `.pkg.tar.zst`.
    pub sha256: String,
    /// Taille compressée (octets).
    pub csize: u64,
    /// Nom exact du fichier paquet dans le dépôt (ex.
    /// `linux-cachyos-7.3.0-4-x86_64_v4.pkg.tar.zst`).
    pub filename: String,
    /// Libellé descriptif court (le `DESC`).
    #[allow(dead_code)]
    pub description: String,
}

impl KernelRelease {
    /// URL complète du paquet dans le dépôt (base du dépôt + 1er sous-dépôt + nom).
    pub fn url(&self) -> String {
        let base = repo_base(self.kind);
        let sub = repo_subdirs(self.kind)[0];
        format!("{}/{}/{}", base, sub, self.filename)
    }
    /// Chemin du fichier téléchargé en cache local.
    pub fn cache_path(&self) -> PathBuf {
        crate::sysutil::state_dir().join(self.filename.clone())
    }
}

/// Base du dépôt (URL sans le sous-dépôt ni le nom de fichier).
fn repo_base(kind: KernelKind) -> &'static str {
    match kind {
        KernelKind::Cachyos => "https://mirror.cachyos.org/repo",
        KernelKind::Zen => "https://mirror.leaseweb.com/archlinux",
        KernelKind::Liquorix => "https://liquorix.net/archlinux",
    }
}

/// Sous-dépôt relatif (dans l'ordre) ; le premier réussi est retenu.
fn repo_subdirs(kind: KernelKind) -> Vec<&'static str> {
    match kind {
        KernelKind::Cachyos => vec!["x86_64_v4/cachyos-v4", "x86_64/cachyos"],
        KernelKind::Zen => vec!["extra/os/x86_64"],
        KernelKind::Liquorix => vec!["liquorix/x86_64"],
    }
}

/// Nom de la base pacman-db (dans la génération courante).
fn db_name(kind: KernelKind) -> &'static str {
    match kind {
        // CachyOS : la base de la génération `cachyos-v4` (x86_64_v4) se nomme
        // `cachyos-v4.db` ; la génération `cachyos` utilisait `cachyos.db`.
        KernelKind::Cachyos => "cachyos-v4.db",
        KernelKind::Zen => "extra.db",
        KernelKind::Liquorix => "liquorix.db",
    }
}

fn agent() -> Agent {
    ureq::AgentBuilder::new()
        .user_agent("solus-external-kernel-manager/0.1 (Solus)")
        .timeout(Duration::from_secs(120))
        .build()
}

/// Récupère les trois dernières versions du noyau `kind` présentes dans le
/// dépôt, triées du plus récent au plus ancien.
pub fn fetch_latest_kernels(kind: KernelKind) -> Result<Vec<KernelRelease>, String> {
    let agent = agent();

    // Les dépôts pacman peuvent changer de génération (CachyOS a déjà passé
    // de `x86_64/cachyos` à `x86_64_v4/cachyos-v4`, la base étant
    // `cachyos.db` → `cachyos-v4.db`) : on essaie chaque sous-dépôt jusqu'à
    // obtenir une base lisible.
    let mut last_err = String::new();
    let mut tar_bytes: Option<Vec<u8>> = None;
    for sub in repo_subdirs(kind) {
        let dbf = db_name(kind);
        let url = format!("{}/{}/{}", repo_base(kind), sub, dbf);
        log_verbose(&format!("[{}] Téléchargement de la base : {}", kind.label(), url));
        match agent.get(&url).call() {
            Ok(resp) => {
                let mut bytes = Vec::new();
                if let Err(e) = resp.into_reader().read_to_end(&mut bytes) {
                    last_err = format!("Lecture de {} impossible : {}", url, e);
                    continue;
                }
                if bytes.is_empty() {
                    last_err = format!("Base {} vide", url);
                    continue;
                }
                log_verbose(&format!(
                    "[{}] Base téléchargée : {} o (signature {:02x}{:02x}{:02x}{:02x})",
                    kind.label(),
                    bytes.len(),
                    bytes.get(0).copied().unwrap_or(0),
                    bytes.get(1).copied().unwrap_or(0),
                    bytes.get(2).copied().unwrap_or(0),
                    bytes.get(3).copied().unwrap_or(0)
                ));
                // Une page HTML 404 déguisée en 200 : on la rejette.
                if bytes.len() >= 5 && &bytes[0..5] == b"<html" {
                    last_err = format!("Base {} = page HTML (dépôt absent?)", url);
                    continue;
                }
                match decompress_db(&bytes).map_err(|e| {
                    last_err = format!("Décompression de {} : {}", url, e);
                    e
                }) {
                    Ok(tb) => {
                        tar_bytes = Some(tb);
                        break;
                    }
                    Err(e) => last_err = e,
                }
            }
            Err(e) => last_err = format!("Échec du téléchargement de {} : {}", url, e),
        }
    }
    let tar = tar_bytes.ok_or(last_err)?;

    // Parcourt les entries `<pkg>-<ver>/desc` du tar et parse celles qui
    // décrivent le paquet noyau principal (ex. `linux-zen`, `linux-lqx`).
    let mut releases = Vec::new();
    let mut archive = Archive::new(tar.as_slice());
    for entry in archive.entries().map_err(|e| e.to_string())? {
        let mut e = entry.map_err(|e| e.to_string())?;
        let path = e.path().map_err(|e| e.to_string())?.to_string_lossy().to_string();
        // Un enregistrement pacman-db = <pkg>-<ver>/desc ; le nom du paquet
        // est dans les tags %NAME% / %VERSION% du desc.
        let parts: Vec<&str> = path.split('/').collect();
        if parts.len() != 2 || parts[1] != "desc" {
            continue;
        }
        let mut desc = String::new();
        e.read_to_string(&mut desc).map_err(|e| e.to_string())?;
        if let Some(k) = parse_desc(kind, parts[0], &desc) {
            releases.push(k);
        }
    }

    if releases.is_empty() {
        return Err(format!(
            "Aucun paquet {} trouvé dans le dépôt.",
            kind.pkg_name()
        ));
    }
    // Trie du plus récent au plus ancien (version pacman lexicographique),
    // puis on garde les 3.
    releases.sort_by(|a, b| b.version.cmp(&a.version));
    Ok(releases.into_iter().take(3).collect())
}

/// Décompresse une base pacman-db selon son format (zstd / xz) et retourne
/// les octets du tar brut.
fn decompress_db(bytes: &[u8]) -> Result<Vec<u8>, String> {
    match detect_db_format(bytes) {
        DbFormat::Zstd => {
            log_verbose("[repo] Base au format zstd…");
            zstd::decode_all(bytes)
                .map_err(|e| format!("Décompression zstd impossible : {}", e))
        }
        DbFormat::Xz => {
            log_verbose("[repo] Base au format xz…");
            let mut out = Vec::new();
            let mut dec = XzDecoder::new(bytes);
            dec.read_to_end(&mut out)
                .map_err(|e| format!("Décompression xz impossible : {}", e))?;
            Ok(out)
        }
        DbFormat::Gzip => {
            log_verbose("[repo] Base au format gzip…");
            let mut out = Vec::new();
            let mut dec = GzDecoder::new(bytes);
            dec.read_to_end(&mut out)
                .map_err(|e| format!("Décompression gzip impossible : {}", e))?;
            Ok(out)
        }
        DbFormat::Unknown => Err(format!(
            "Format de la base pacman-db inconnu (ni zstd ni xz) : {:02x?}…",
            &bytes[..4.min(bytes.len())]
        )),
    }
}

/// Extrait le tag pacman `%TAG%` suivi de sa valeur (ligne suivante).
pub(crate) fn extract_tag(desc: &str, tag: &str) -> String {
    // Format pacman-db :
    //   %TAG%
    //   value
    //   (ligne vide)
    let mut lines = desc.split('\n');
    while let Some(line) = lines.next() {
        if line == tag {
            return lines.next().unwrap_or("").trim().to_string();
        }
    }
    String::new()
}

/// Retire le suffixe release pacman « -N » d'une version Arch quand la partie
/// qui précède le `-` contient des lettres (pattern Arch `X.Y.Z.<feature>-N`),
/// pour obtenir la version du noyau :
///   - `7.2.11.zen1-1` → `7.2.11.zen1`  (précède par « zen1 » → lettres)
///   - `7.2.12.lqx1-1` → `7.2.12.lqx1`  (précédé par « lqx1 » → lettres)
///   - `7.3.0-4`       → `7.3.0-4`      (précédé uniquement de chiffres : intact)
fn strip_arch_release(version: &str) -> String {
    // Le suffixe release Arch est `-<number>` ; on ne le coupe que si la partie
    // AVANT ce dernier `-` contient au moins une lettre (le « feature » :
    // zen1, lqx1…). La version CachyOS `7.3.0-4` n'a que des chiffres avant
    // le `-` → on ne la touche pas (`-4` fait partie du noyau).
    if let Some(dash) = version.rfind('-') {
        let before = &version[..dash];
        let after = &version[dash + 1..];
        if !after.is_empty()
            && after.chars().all(|c| c.is_ascii_digit())
            && before.chars().any(|c| c.is_ascii_alphabetic())
        {
            return before.to_string();
        }
    }
    version.to_string()
}

/// Interprète un enregistrement `desc` du dépôt pacman et produit la
/// [`KernelRelease`] correspondante.
///
/// Retourne `None` si l'enregistrement ne décrit pas le noyau attendu
/// (`kind.pkg_name()`), si le nom du paquet ne correspond pas au répertoire
/// `dir_name`, ou si `%VERSION%` est manquant.
///
/// `dir_name` est la partie avant `/desc` de l'entrée tar, ex.
/// `linux-zen-7.2.11.zen1-1`.
pub fn parse_desc(kind: KernelKind, dir_name: &str, desc: &str) -> Option<KernelRelease> {
    let pkg_name = extract_tag(desc, "%NAME%");
    if pkg_name.is_empty() {
        return None;
    }
    // On ne garde que le paquet noyau attendu (pas `-lts`, `-bmq`, …).
    if pkg_name != kind.pkg_name() {
        return None;
    }
    // Le répertoire du paquet doit commencer par son nom (paranoïa).
    if !dir_name.starts_with(&format!("{}-", pkg_name)) {
        return None;
    }
    let mut version = extract_tag(desc, "%VERSION%");
    if version.is_empty() {
        return None;
    }
    version = version.split(':').last().unwrap_or(&version).to_string();
    // Le pacman version contient un suffixe release `-N` (ex. `-1`) qui ne
    // fait PAS partie de la version du noyau. On le retire :
    //   `7.2.11.zen1-1` → `7.2.11.zen1`   `7.3.0-4` → `7.3.0-4` (déjà bon)
    // `7.3.0-4` ne doit PAS être tronqué (c'est déjà le noyau complet cachyos)
    // ; on n'enlève le suffixe qu'est-ce qui suit un `.` (pattern Arch) :
    let kernel_version_base = strip_arch_release(&version);

    let sha = extract_tag(desc, "%SHA256SUM%");
    let csize = extract_tag(desc, "%CSIZE%").parse::<u64>().unwrap_or(0);
    let desc_short = extract_tag(desc, "%DESC%");

    // Nom exact du fichier paquet dans le dépôt ; en secours (bases très
    // anciennes sans %FILENAME%), on reconstruit la formule pacman standard.
    let filename = extract_tag(desc, "%FILENAME%");
    let filename = if filename.is_empty() {
        format!("{}-{}-x86_64.pkg.tar.zst", pkg_name, version)
    } else {
        filename
    };

    // Version d'affichage du noyau (le dossier modules réel est découvert
    // depuis l'archive — voir discover_pkg_layout).
    let kernel_version = match kind {
        KernelKind::Cachyos => format!("{}-cachyos", version),
        KernelKind::Zen => kernel_version_base,
        KernelKind::Liquorix => format!("{}-lqx", kernel_version_base),
    };

    Some(KernelRelease {
        kind,
        pkg_name,
        version,
        kernel_version,
        sha256: sha,
        csize,
        filename,
        description: desc_short,
    })
}

/// Télécharge le paquet noyau vers le cache local en vérifiant le SHA256.
/// Retourne le chemin du fichier.
pub fn download_kernel(
    k: &KernelRelease,
    on_progress: &dyn Fn(u64, u64),
) -> Result<PathBuf, String> {
    let path = k.cache_path();
    if path.exists() {
        // Vérification rapide : on ne re-télécharge que si le fichier est invalide.
        if verify_sha256(&path, &k.sha256).unwrap_or(false) {
            on_progress(path.metadata().map(|m| m.len()).unwrap_or(0), k.csize);
            return Ok(path);
        }
        let _ = std::fs::remove_file(&path);
    }

    let agent = agent();
    let url = k.url();
    crate::sysutil::log(&format!(
        "Téléchargement {} ({} Mo) …",
        url,
        k.csize / 1_000_000
    ));
    let resp = agent
        .get(&url)
        .call()
        .map_err(|e| format!("Échec du téléchargement : {}", e))?;

    let total = resp
        .header("content-length")
        .and_then(|v| v.parse().ok())
        .unwrap_or(k.csize);
    let mut out = std::fs::File::create(&path).map_err(|e| e.to_string())?;
    let mut reader = resp.into_reader();
    let mut received: u64 = 0;
    let mut buf = vec![0u8; 256 * 1024];
    loop {
        let n = reader.read(&mut buf).map_err(|e| e.to_string())?;
        if n == 0 {
            break;
        }
        use std::io::Write;
        out.write_all(&buf[..n]).map_err(|e| e.to_string())?;
        received += n as u64;
        on_progress(received, total);
    }
    drop(out);

    let ok = verify_sha256(&path, &k.sha256)?;
    if !ok {
        let _ = std::fs::remove_file(&path);
        return Err(format!(
            "SHA256 invalide pour {} (attendu {})",
            k.pkg_name, k.sha256
        ));
    }
    Ok(path)
}

fn verify_sha256(path: &std::path::Path, expected: &str) -> Result<bool, String> {
    use sha2::{Digest, Sha256};
    let data = std::fs::read(path).map_err(|e| e.to_string())?;
    let mut h = Sha256::new();
    h.update(&data);
    let got = hex::encode(h.finalize());
    Ok(got == expected.to_ascii_lowercase())
}

fn log_verbose(msg: &str) {
    crate::sysutil::log(msg);
}

/// Utilitaire : vérifie qu'un binaire est disponible.
#[allow(dead_code)]
pub fn has_bin(name: &str) -> bool {
    Command::new("which")
        .arg(name)
        .output()
        .map(|o| o.status.success())
        .unwrap_or(false)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Un enregistrement `desc` réaliste pour CachyOS (génération v4).
    const DESC_CACHYOS: &str = r#"%CSIZE%
149245152
%FILENAME%
linux-cachyos-7.3.0-4-x86_64_v4.pkg.tar.zst
%NAME%
linux-cachyos
%SHA256SUM%
db86135e11054d44f1b2a9b1b2c3d4e5f6a1b2c3d4e5f6a1b2c3d4e5f6a1b2c3
%DESC%
The Linux kernel provided by CachyOS.
%VERSION%
7.3.0-4
%"#;

    /// Un enregistrement `desc` réaliste pour Zen (Arch `extra`).
    const DESC_ZEN: &str = r#"%CSIZE%
175000000
%FILENAME%
linux-zen-7.2.11.zen1-1-x86_64.pkg.tar.zst
%NAME%
linux-zen
%SHA256SUM%
a1b2c3d4e5f6a1b2c3d4e5f6a1b2c3d4e5f6a1b2c3d4e5f6a1b2c3d4e5f6a1b2
%DESC%
The Zen Kernel, optimized for workstations and desktops.
%VERSION%
7.2.11.zen1-1
%"#;

    /// Un enregistrement `desc` réaliste pour Liquorix.
    const DESC_LQX: &str = r#"%CSIZE%
146000000
%FILENAME%
linux-lqx-7.2.12.lqx1-1-x86_64.pkg.tar.zst
%NAME%
linux-lqx
%SHA256SUM%
c3d4e5f6a1b2c3d4e5f6a1b2c3d4e5f6a1b2c3d4e5f6a1b2c3d4e5f6a1b2c3d4
%DESC%
The Liquorix kernel, low latency for audio/gaming.
%VERSION%
7.2.12.lqx1-1
%"#;

    #[test]
    fn detects_zstd_magic() {
        assert_eq!(
            detect_db_format(&[0x28, 0xb5, 0x2f, 0xfd, 0x00, 0x01]),
            DbFormat::Zstd
        );
    }

    #[test]
    fn detects_xz_magic() {
        assert_eq!(
            detect_db_format(&[0xfd, 0x37, 0x7a, 0x58, 0x5a, 0x00, 0x01]),
            DbFormat::Xz
        );
    }

    #[test]
    fn detects_html_or_unknown() {
        assert_eq!(
            detect_db_format(b"<html><body>404</body></html>"),
            DbFormat::Unknown
        );
        assert_eq!(detect_db_format(&[0x28, 0xb5]), DbFormat::Unknown);
        assert_eq!(detect_db_format(&[]), DbFormat::Unknown);
    }

    #[test]
    fn extract_tag_reads_value_line() {
        let desc = "%SHA256SUM%\nab12cd34\n%CSIZE%\n123\n";
        assert_eq!(extract_tag(desc, "%SHA256SUM%"), "ab12cd34");
        assert_eq!(extract_tag(desc, "%CSIZE%"), "123");
        assert_eq!(extract_tag(desc, "%NOMEXISTANT%"), "");
        let two = "%A%\n1\n%A%\n2\n";
        assert_eq!(extract_tag(two, "%A%"), "1");
    }

    #[test]
    fn parse_desc_cachyos_full() {
        let k = parse_desc(
            KernelKind::Cachyos,
            "linux-cachyos-7.3.0-4",
            DESC_CACHYOS,
        )
        .expect("desc cachyos valide");
        assert_eq!(k.kind, KernelKind::Cachyos);
        assert_eq!(k.pkg_name, "linux-cachyos");
        assert_eq!(k.version, "7.3.0-4");
        assert_eq!(k.kernel_version, "7.3.0-4-cachyos");
        assert_eq!(k.csize, 149_245_152);
        assert_eq!(
            k.filename,
            "linux-cachyos-7.3.0-4-x86_64_v4.pkg.tar.zst"
        );
        assert!(k.sha256.starts_with("db86135e"));
        assert!(k.url().contains("/x86_64_v4/cachyos-v4/"));
    }

    #[test]
    fn parse_desc_zen_full() {
        let k = parse_desc(
            KernelKind::Zen,
            "linux-zen-7.2.11.zen1-1",
            DESC_ZEN,
        )
        .expect("desc zen valide");
        assert_eq!(k.kind, KernelKind::Zen);
        assert_eq!(k.pkg_name, "linux-zen");
        assert_eq!(k.version, "7.2.11.zen1-1");
        // Pour Arch (Zen), le kernel_version = version pacman SANS le suffixe
        // release `-1`.
        assert_eq!(k.kernel_version, "7.2.11.zen1");
        assert!(k.url().contains("/extra/os/x86_64/"));
    }

    #[test]
    fn parse_desc_lqx_full() {
        let k = parse_desc(
            KernelKind::Liquorix,
            "linux-lqx-7.2.12.lqx1-1",
            DESC_LQX,
        )
        .expect("desc lqx valide");
        assert_eq!(k.kind, KernelKind::Liquorix);
        assert_eq!(k.pkg_name, "linux-lqx");
        assert_eq!(k.version, "7.2.12.lqx1-1");
        // Libellé d'affichage : le pacman `7.2.12.lqx1` devient
        // `7.2.12.lqx1-lqx` (le dossier modules réel est découvert depuis
        // l'archive).
        assert_eq!(k.kernel_version, "7.2.12.lqx1-lqx");
        assert!(k.url().contains("/liquorix/x86_64/"));
    }

    #[test]
    fn parse_desc_rejects_other_package() {
        // Un Zen ne doit PAS être retenu pour CachyOS.
        assert!(parse_desc(KernelKind::Cachyos, "linux-zen-7.2.11.zen1-1", DESC_ZEN).is_none());
        // Un CachyOS ne doit PAS être retenu pour Liquorix.
        assert!(
            parse_desc(KernelKind::Liquorix, "linux-cachyos-7.3.0-4", DESC_CACHYOS).is_none()
        );
    }

    #[test]
    fn parse_desc_rejects_dir_name_mismatch() {
        assert!(
            parse_desc(
                KernelKind::Cachyos,
                "linux-other-7.3.0",
                DESC_CACHYOS
            )
            .is_none()
        );
    }

    #[test]
    fn parse_desc_rejects_missing_version() {
        let nover = DESC_CACHYOS.replace("%VERSION%\n7.3.0-4", "%VERSION%\n");
        assert!(
            parse_desc(KernelKind::Cachyos, "linux-cachyos-7.3.0-4", &nover).is_none()
        );
    }

    #[test]
    fn parse_desc_falls_back_filename() {
        let nofile = DESC_ZEN.replace(
            "%FILENAME%\nlinux-zen-7.2.11.zen1-1-x86_64.pkg.tar.zst\n",
            "",
        );
        let k = parse_desc(
            KernelKind::Zen,
            "linux-zen-7.2.11.zen1-1",
            &nofile,
        )
        .expect("fallback filename");
        assert_eq!(k.filename, "linux-zen-7.2.11.zen1-1-x86_64.pkg.tar.zst");
    }

    #[test]
    fn kind_labels() {
        assert_eq!(KernelKind::Cachyos.label(), "CachyOS");
        assert_eq!(KernelKind::Zen.label(), "Zen");
        assert_eq!(KernelKind::Liquorix.label(), "Liquorix");
        assert_eq!(KernelKind::Cachyos.pkg_name(), "linux-cachyos");
        assert_eq!(KernelKind::Zen.pkg_name(), "linux-zen");
        assert_eq!(KernelKind::Liquorix.pkg_name(), "linux-lqx");
    }

    #[test]
    fn strip_arch_release_strips_alpha_feature_dash_num() {
        assert_eq!(strip_arch_release("7.2.11.zen1-1"), "7.2.11.zen1");
        assert_eq!(strip_arch_release("7.2.12.lqx1-1"), "7.2.12.lqx1");
    }

    #[test]
    fn strip_arch_release_keeps_digits_only() {
        // La version CachyOS `7.3.0-4` a un chiffre avant le `-` → intact.
        assert_eq!(strip_arch_release("7.3.0-4"), "7.3.0-4");
        assert_eq!(strip_arch_release("6.1.0-1"), "6.1.0-1");
    }

    #[test]
    fn strip_arch_release_no_dash() {
        assert_eq!(strip_arch_release("7.2.11.zen1"), "7.2.11.zen1");
    }
}
