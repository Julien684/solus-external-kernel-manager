//! Utilitaires système : sudo, lecture de fichiers, détection d'état.

use std::fs;
use std::path::PathBuf;
use std::process::{Command, Stdio};

/// Lier un chemin absolu de l'utilisateur.
fn home_dir() -> PathBuf {
    PathBuf::from(std::env::var("HOME").unwrap_or_else(|_| "/root".to_string()))
}

/// Chemin du dossier d'état de l'application.
pub fn state_dir() -> PathBuf {
    let p = home_dir().join(".local/state/solus-external-kernel-manager");
    let _ = fs::create_dir_all(&p);
    p
}

pub fn log_file() -> PathBuf {
    state_dir().join("manager.log")
}

/// Appender une ligne horodatée dans le journal (et stdout).
pub fn log(line: &str) {
    let ts = chrono::Local::now().format("%Y-%m-%d %H:%M:%S");
    let full = format!("[{}] {}", ts, line);
    println!("{}", full);
    if let Ok(mut f) = fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(log_file())
    {
        use std::io::Write;
        let _ = f.write_all(full.as_bytes());
        let _ = f.write_all(b"\n");
    }
}

/// Espace libre (octets) : `f_bavail` (blocs non-root) × taille du bloc.
#[allow(clippy::cast_sign_loss)]
pub(crate) fn free_in_statfs(st: &Statfs) -> u64 {
    st.f_bavail.max(0) as u64 * st.f_bsize.max(0) as u64
}

/// Capacité totale (octets) : `f_blocks` × taille du bloc.
#[allow(clippy::cast_sign_loss)]
pub(crate) fn total_in_statfs(st: &Statfs) -> u64 {
    st.f_blocks.max(0) as u64 * st.f_bsize.max(0) as u64
}

#[derive(Debug)]
pub(crate) struct Statfs {
    pub f_bsize: i64,   // taille d'un bloc (octets)
    pub f_bavail: i64,  // blocs disponibles pour un utilisateur (non-root)
    pub f_blocks: i64,  // blocs au total
}

/// `statvfs` d'un chemin quelconque (montage).
fn statvfs_at(path: &str) -> Option<Statfs> {
    let mut sv: libc::statvfs = unsafe { std::mem::zeroed() };
    let cpath = std::ffi::CString::new(path).ok()?;
    let r = unsafe { libc::statvfs(cpath.as_ptr(), &mut sv) };
    if r != 0 {
        return None;
    }
    Some(Statfs {
        f_bsize: sv.f_bsize as i64,
        f_bavail: sv.f_bavail as i64,
        f_blocks: sv.f_blocks as i64,
    })
}

/// Espace d'une partition de démarrage : total et libre (octets).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct BootSpace {
    pub total: u64,
    pub free: u64,
}

/// Point de montage de `/boot` s'il est réellement monté (distinct de `/`).
fn boot_mountpoint() -> Option<String> {
    let m = fs::read_to_string("/proc/mounts").ok()?;
    m.lines()
        .filter_map(|l| {
            let mut it = l.split_whitespace();
            let _dev = it.next()?;
            let mp = it.next()?;
            Some(mp.to_string())
        })
        .find(|mp| mp == "/boot" || mp == "/boot/efi" || mp == "/efi")
}

/// Périphérique ESP (partition vfat / type « EFI System ») via `lsblk -J -b`.
/// Retourne `(chemin, taille_octets, point_de_montage_éventuel)`.
pub fn esp_device() -> Option<(String, u64, Option<String>)> {
    let out = Command::new("lsblk")
        .args(["-J", "-b", "-o", "NAME,PATH,FSTYPE,PARTTYPE,MOUNTPOINT,SIZE"])
        .output()
        .ok()?;
    if !out.status.success() {
        return None;
    }
    let v: serde_json::Value = serde_json::from_slice(&out.stdout).ok()?;
    let mut found: Vec<(String, u64, Option<String>)> = Vec::new();
    if let Some(arr) = v.get("blockdevices").and_then(|x| x.as_array()) {
        for d in arr {
            walk_lsblk(d, &mut found);
        }
    }
    // Priorité : monté sur /boot*, puis vfat « EFI System », puis vfat.
    found
        .iter()
        .find(|(_, _, mp)| mp.as_deref() == Some("/boot"))
        .or_else(|| found.iter().find(|(_, _, mp)| mp.as_deref() == Some("/boot/efi")))
        .or_else(|| found.iter().find(|(_, _, mp)| mp.as_deref() == Some("/efi")))
        .or_else(|| found.first())
        .cloned()
}

fn walk_lsblk(d: &serde_json::Value, out: &mut Vec<(String, u64, Option<String>)>) {
    let fstype = d.get("fstype").and_then(|x| x.as_str()).unwrap_or("");
    let parttype = d.get("parttype").and_then(|x| x.as_str()).unwrap_or("");
    let path = d.get("path").and_then(|x| x.as_str()).unwrap_or("");
    let size = d.get("size").and_then(|x| x.as_u64()).unwrap_or(0);
    let mp = d
        .get("mountpoint")
        .and_then(|x| x.as_str())
        .filter(|s| !s.is_empty())
        .map(|s| s.to_string());
    let is_efi = parttype.starts_with("c12a7328"); // EFI System Partition
    if !path.is_empty() && (is_efi || fstype.eq_ignore_ascii_case("vfat")) {
        out.push((path.to_string(), size, mp.clone()));
    }
    if let Some(ch) = d.get("children").and_then(|x| x.as_array()) {
        for c in ch {
            walk_lsblk(c, out);
        }
    }
}

/// Espace de la partition de démarrage **sans privilège** : uniquement si elle
/// est déjà montée (`/boot`, `/boot/efi`, `/efi`) ou si l'ESP est montée
/// ailleurs. Retourne `None` si la partition n'est pas montée.
pub fn boot_space_nopass() -> Option<BootSpace> {
    if let Some(mp) = boot_mountpoint() {
        if let Some(st) = statvfs_at(&mp) {
            return Some(BootSpace {
                total: total_in_statfs(&st),
                free: free_in_statfs(&st),
            });
        }
    }
    // ESP montée à un autre endroit ?
    let (_, size, mp) = esp_device()?;
    if let Some(mp) = mp {
        if let Some(st) = statvfs_at(&mp) {
            return Some(BootSpace {
                total: total_in_statfs(&st),
                free: free_in_statfs(&st),
            });
        }
    }
    let _ = size;
    None
}

/// Taille de l'ESP (octets) même non montée — via `lsblk`.
pub fn esp_size() -> Option<u64> {
    esp_device().map(|(_, size, _)| size).filter(|s| *s > 0)
}

/// Espace de la partition de démarrage en montant l'ESP en lecture seule
/// (nécessite root). Monte dans `/run/sekm-boot-ro`, mesure, puis démonte.
/// Ne modifie pas l'état du système (remet la partition non montée).
pub fn boot_space_with_sudo(password: &str) -> Result<BootSpace, String> {
    if let Some(s) = boot_space_nopass() {
        return Ok(s);
    }
    let (dev, _size, _mp) = esp_device().ok_or_else(|| "ESP introuvable (lsblk)".to_string())?;
    let mnt = "/run/sekm-boot-ro";
    let (c, _o, e) = run_sudo(password, "mkdir", &["-p", mnt]);
    if c != 0 {
        return Err(format!("mkdir {} : {}", mnt, e.trim()));
    }
    let (c, _o, e) = run_sudo(password, "mount", &["-o", "ro", &dev, mnt]);
    if c != 0 {
        let _ = run_sudo(password, "rmdir", &[mnt]);
        return Err(format!("mount ro {} : {}", dev, e.trim()));
    }
    let res = statvfs_at(mnt);
    let _ = run_sudo(password, "umount", &[mnt]);
    let _ = run_sudo(password, "rmdir", &[mnt]);
    let st = res.ok_or_else(|| format!("statvfs {} impossible", mnt))?;
    Ok(BootSpace {
        total: total_in_statfs(&st),
        free: free_in_statfs(&st),
    })
}

/// Version du noyau en cours d'exécution (ex. `7.2.11-366.current`).
pub fn running_kernel() -> String {
    std::fs::read_to_string("/proc/sys/kernel/osrelease")
        .map(|s| s.trim().to_string())
        .unwrap_or_else(|_| "inconnu".into())
}

/// Suffixe qui identifie un noyau externe, déduit de sa version.
/// Ex. `7.3.0-4-cachyos` → `-cachyos` ; `7.2.11.zen1` → `.zen1` ;
/// `7.2.12.lqx1-lqx` → `-lqx`. Vide pour un noyau inconnu.
pub fn kernel_suffix(kernel_version: &str) -> String {
    let v = kernel_version.to_ascii_lowercase();
    if let Some(i) = v.rfind("-cachyos") {
        return v[i..].to_string();
    }
    if let Some(i) = v.find(".zen") {
        return v[i..].to_string();
    }
    if let Some(i) = v.rfind("-lqx") {
        return v[i..].to_string();
    }
    String::new()
}

/// Sous-répertoire EFI « propriétaire » des noyaux donnés (pour l'isoler
/// de l'ESP géré par `clr-boot-manager`).
pub fn kernel_vendor_dir(kernel_version: &str) -> String {
    let s = kernel_suffix(kernel_version);
    if s.contains("cachyos") {
        "com.cachyos".into()
    } else if s.contains("zen") {
        "com.zen-kernel".into()
    } else if s.contains("lqx") {
        "com.liquorix".into()
    } else {
        "com.external".into()
    }
}

/// Suffixe « legacy » — déduction du type d'un noyau **d'après son nom de
/// dossier modules** (sans le registre d'installation).
///
/// NOTE : ne marche qu'avec les noms de dossiers « classiques »
/// (`…-cachyos`, `…zen…`, `…-lqx`). Les paquets pacman récents nomment le
/// dossier modules avec le release pacman complet, ex.
/// `7.2.11-zen1-1-zen` (sans point avant `zen`) → non reconnu. La source de
/// vérité est donc le registre [`registry_path`], alimenté à l'installation.
#[allow(dead_code)]
pub fn legacy_kernel_suffix(name: &str) -> String {
    let v = name.to_ascii_lowercase();
    if let Some(i) = v.rfind("-cachyos") {
        return v[i..].to_string();
    }
    if v.contains("zen") {
        return "zen".to_string();
    }
    if let Some(i) = v.rfind("-lqx") {
        return v[i..].to_string();
    }
    String::new()
}

/// Devine le type du noyau à partir de son nom de dossier modules (repli
/// quand le registre n'a pas d'entrée — noyaux installés par une ancienne
/// version sans registre).
///
/// `7.3.0-4-cachyos` → CachyOS, `7.2.11-zen1-1-zen` → Zen,
/// `7.2.12-lqx1-1-lqx` → Liquorix. `None` si inconnu.
pub fn guess_kind(name: &str) -> Option<crate::repo::KernelKind> {
    use crate::repo::KernelKind;
    let v = name.to_ascii_lowercase();
    if v.contains("cachyos") {
        Some(KernelKind::Cachyos)
    } else if v.contains("zen") {
        Some(KernelKind::Zen)
    } else if v.contains("lqx") {
        Some(KernelKind::Liquorix)
    } else {
        None
    }
}

// ----------------------------------------------------------------------
// Registre d'installation
// ----------------------------------------------------------------------
//
// Les noms de dossiers `/lib/modules/<…>` des paquets pacman récents ne laissent
// pas deviner le fournisseur (ex. `7.2.11-zen1-1-zen` pour Zen). Pour pouvoir
// LISTER et SUPPRIMER fiablement les noyaux installés, l'application tient un
// petit registre JSON : `{"<nom modules>": "kind"}`.
//  - écrit à chaque installation (`record_install`) ;
// - consulté par `list_installed` et `remove_kernel` ;
// - nettoyé à la suppression (`forget_install`).

use std::collections::HashMap;

/// Chemin du registre d'installation.
pub fn registry_path() -> PathBuf {
    state_dir().join("registry.json")
}

/// Une entrée du registre : le type du noyau + sa version d'affichage
/// (celle du dépôt pacman, ex. `7.2.11.zen1`), distincte du nom du dossier
/// modules (`7.2.11-zen1-1-zen`).
#[derive(serde::Serialize, serde::Deserialize)]
pub struct RegistryEntry {
    pub kind: String,
    #[serde(default)]
    pub display: Option<String>,
}

/// Lit le registre ; `{}` s'il est absent ou illisible.
pub fn read_registry() -> HashMap<String, RegistryEntry> {
    let content = match fs::read_to_string(registry_path()) {
        Ok(c) => c,
        Err(_) => return HashMap::new(),
    };
    serde_json::from_str::<HashMap<String, RegistryEntry>>(&content)
        .unwrap_or_default()
}

/// Écrit le registre atomiquement (tmp + rename).
fn write_registry(map: &HashMap<String, RegistryEntry>) {
    let path = registry_path();
    let tmp = path.with_extension("json.tmp");
    if let Ok(json) = serde_json::to_string_pretty(map) {
        let _ = fs::write(&tmp, json + "\n");
        let _ = fs::rename(&tmp, path);
    }
}

/// Enregistre qu'un noyau (nom de dossier modules `name`) a été installé.
/// `display` est la version d'affichage du dépôt (ex. `7.2.11.zen1`) —
/// sert à marquer « installée » dans la table des versions.
pub fn record_install(name: &str, kind: crate::repo::KernelKind, display: Option<&str>) {
    let mut map = read_registry();
    map.insert(
        name.to_string(),
        RegistryEntry {
            kind: kind.label().to_string(),
            display: display.map(|s| s.to_string()),
        },
    );
    write_registry(&map);
}

/// Type d'un noyau installé, d'après le registre.
pub fn registry_kind(name: &str) -> Option<crate::repo::KernelKind> {
    read_registry()
        .get(name)
        .and_then(|e| crate::repo::KernelKind::from_label(e.kind.as_str()))
}

/// Version d'affichage enregistrée (None si absente).
#[allow(dead_code)]
pub fn registry_display(name: &str) -> Option<String> {
    read_registry().get(name).and_then(|e| e.display.clone())
}

/// Ote une entrée du registre (après suppression).
pub fn forget_install(name: &str) {
    let mut map = read_registry();
    if map.remove(name).is_some() {
        write_registry(&map);
    }
}

/// Chemins d'installation d'un noyau externe (version `v`).
pub struct KernelPaths {
    pub vmlinuz: PathBuf,
    pub initrd: PathBuf,
    pub modules: PathBuf,
    pub boot_entry: PathBuf,
}

impl KernelPaths {
    /// Chemins d'installation pour un noyau externe.
    ///
    /// `kname` est le **nom du dossier modules** découvert dans le paquet
    /// (= `uname -r` du noyau, ex. `7.2.12-lqx1-1-lqx`) ; `kind` fournit le
    /// sous-répertoire EFI propriétaire.
    pub fn for_kernel(kname: &str, kind: crate::repo::KernelKind) -> Self {
        let ven = kind.vendor_dir();
        Self {
            vmlinuz: PathBuf::from(format!("/boot/EFI/{}/kernel-{}", ven, kname)),
            initrd: PathBuf::from(format!("/boot/EFI/{}/initrd-{}", ven, kname)),
            modules: PathBuf::from(format!("/lib/modules/{}", kname)),
            boot_entry: PathBuf::from(format!("/boot/loader/entries/ext-{}.conf", kname)),
        }
    }
    #[allow(dead_code)]
    pub fn for_version(v: &str) -> Self {
        let ven = kernel_vendor_dir(v);
        Self {
            vmlinuz: PathBuf::from(format!("/boot/EFI/{}/kernel-{}", ven, v)),
            initrd: PathBuf::from(format!("/boot/EFI/{}/initrd-{}", ven, v)),
            modules: PathBuf::from(format!("/lib/modules/{}", v)),
            boot_entry: PathBuf::from(format!("/boot/loader/entries/ext-{}.conf", v)),
        }
    }
    #[allow(dead_code)]
    pub fn is_installed(&self) -> bool {
        self.vmlinuz.exists() && self.modules.exists()
    }
}

/// Liste des noyaux externes déjà installés sur la machine (CachyOS, Zen,
/// Liquorix), triés (plus vieux d'abord).
///
/// On lit `/lib/modules` (sur la racine, toujours accessible) et non
/// `/boot/EFI/…` : en usage courant `/boot` n'est PAS monté
/// (`clr-boot-manager` ne le monte qu'à la demande). Lire /boot faisait donc
/// apparaître une liste vide alors que le noyau était bien installé —
/// impossible de le sélectionner pour suppression quand on tourne sur Solus.
///
/// Le filtrage « noyau externe » passe par [`guess_kind`], qui reconnaît les
/// noms de dossiers pacman récents (`7.2.11-zen1-1-zen`, …) ; le simple
/// `kernel_suffix` (heuristique legacy) les ratait — c'était le bug « Zen
/// invisible dans la liste ».
pub fn installed_ext_kernels() -> Vec<String> {
    let mut out = Vec::new();
    if let Ok(dir) = fs::read_dir("/lib/modules") {
        for e in dir.flatten() {
            let name = e.file_name().to_string_lossy().to_string();
            if e.path().is_dir() && guess_kind(&name).is_some() {
                out.push(name);
            }
        }
    }
    out.sort();
    out
}

/// Sous-ensemble des noyaux installés partageant le même type (suffixe).
#[allow(dead_code)]
pub fn installed_ext_kernels_suffix(sample_kernel_version: &str) -> Vec<String> {
    let s = kernel_suffix(sample_kernel_version);
    if s.is_empty() {
        return installed_ext_kernels();
    }
    installed_ext_kernels()
        .into_iter()
        .filter(|v| kernel_suffix(v) == s)
        .collect()
}

/// Lit le délai du menu systemd-boot (`clr-boot-manager get-timeout`).
/// Nécessite root → mot de passe fourni par l'appelant.
pub fn boot_timeout_get(password: &str) -> Result<String, String> {
    let (code, out, err) = run_sudo(password, "clr-boot-manager", &["get-timeout"]);
    if code != 0 {
        return Err(format!(
            "get-timeout a échoué (code {}): {}",
            code,
            err.trim()
        ));
    }
    let o = out.trim();
    Ok(format_timeout_output(o))
}

/// Rend la sortie brute de `clr-boot-manager get-timeout` lisible :
///   « Timeout value: 8 seconds »            → « 8 secondes »
///   « No timeout is currently configured »  → « non configuré (valeur par défaut) »
pub fn format_timeout_output(raw: &str) -> String {
    let o = raw.trim();
    if o.is_empty() {
        return "inconnu".into();
    }
    if o.contains("No timeout is currently configured") {
        return "non configuré (valeur par défaut)".into();
    }
    if let Some(rest) = o.strip_prefix("Timeout value:") {
        let num: String = rest
            .trim()
            .chars()
            .take_while(|c| c.is_ascii_digit())
            .collect();
        if !num.is_empty() {
            return format!("{} secondes", num);
        }
    }
    o.to_string()
}

/// Définit le délai du menu systemd-boot (`clr-boot-manager set-timeout N`).
/// Nécessite root → mot de passe fourni par l'appelant.
pub fn boot_timeout_set(password: &str, seconds: u32) -> Result<String, String> {
    let s = seconds.to_string();
    let (code, out, err) = run_sudo(password, "clr-boot-manager", &["set-timeout", &s]);
    if code != 0 {
        return Err(format!(
            "set-timeout a échoué (code {}): {}",
            code,
            err.trim()
        ));
    }
    Ok(out.trim().to_string())
}

/// Récupère le mot de passe root (une fois, à la main du thread UI), masqué
/// en points. Laisse `None` si l'utilisateur annule.
///
/// Construit **manuellement** via FFI (pas de builder `Dialog::build()`) car
/// ce dernier crée un `VBoxLayout` local dont le `Drop` supprime le layout C++
/// en fin de fonction, orphelinant les widgets (label tronqué, champ invisible
/// — même bug que la fenêtre principale). Ici, le layout est conservé en local
/// et son C++ n'est supprimé qu'au cleanup (via `QDialog_delete` qui cascade).
pub fn ask_root_password() -> Option<String> {
    use cxx::let_cxx_string;
    use qtrs::ffi;
    use qtrs::AsWidget;
    use qtrs::layout::VBoxLayout;
    use qtrs::{Label, LineEdit};
    use qtrs::dialogbuttonbox::{DialogButtonBox, OK_BUTTON, CANCEL_BUTTON};
    // Qt QLineEdit::EchoMode : 0=Normal, 2=Password.
    const ECHO_PASSWORD: i32 = 2;

    // 1. Dialogue top-level modal (pas de builder : le layout serait dropé).
    let dlg = unsafe { ffi::QDialog_new(std::ptr::null_mut()) };
    unsafe {
        ffi::QDialog_setModal(dlg, true);
        ffi::QDialog_setMinimumSize(dlg, 460, 160);
        ffi::QDialog_resize(dlg, 460, 160);
    }

    // 2. Layout conservé en local → son C++ vit jusqu'au cleanup.
    let mut layout = VBoxLayout::new();
    layout.set_spacing(10);
    layout.set_contents_margins(16, 16, 16, 16);
    unsafe { ffi::QDialog_setLayout(dlg, layout.layout_ptr() as *mut ffi::QLayout); }

    // 3. Widgets (ajout au layout → has_parent=true → le Rust ne supprime pas
    //    le C++; Qt gère la durée de vie via le layout → le dialogue).
    layout.add(Label::new("Mot de passe root (actions administrateur) :").build());

    let edit = LineEdit::new("").echo_mode(ECHO_PASSWORD).build();
    let edit_ptr = edit.widget_ptr() as *mut ffi::QLineEdit;
    layout.add(edit);

    // 3b. Boutons OK/Annuler, câblés EXPLICITEMENT à accept()/reject().
    //     QDialogButtonBox n'est pas relié automatiquement au QDialog (ni par
    //     Qt, ni par qtrs) : sans cette connexion, cliquer OK/Annuler ne fait
    //     rien et `exec()` reste bloqué — le dialogue ne se ferme jamais.
    let buttons = DialogButtonBox::new()
        .standard_buttons(OK_BUTTON | CANCEL_BUTTON)
        .build();
    let ok_ptr = buttons.button_ptr(OK_BUTTON);
    let cancel_ptr = buttons.button_ptr(CANCEL_BUTTON);
    layout.add(buttons);
    {
        let_cxx_string!(sig_clicked = "2clicked()");
        let_cxx_string!(sig_rejected = "2rejected()");
        let_cxx_string!(slot_accept = "1accept()");
        let_cxx_string!(slot_reject = "1reject()");
        let_cxx_string!(slot_clear = "1clear()");
        unsafe {
            let mut ok_conn = false;
            let mut cancel_conn = false;
            if !ok_ptr.is_null() {
                ok_conn = ffi::QObject_connect(
                    ok_ptr as *mut ffi::QObject,
                    &sig_clicked,
                    dlg as *mut ffi::QObject,
                    &slot_accept,
                    0, // Qt::AutoConnection
                );
            }
            if !cancel_ptr.is_null() {
                cancel_conn = ffi::QObject_connect(
                    cancel_ptr as *mut ffi::QObject,
                    &sig_clicked,
                    dlg as *mut ffi::QObject,
                    &slot_reject,
                    0,
                );
            }
            // `QDialog_exec` ne renvoie pas le code de résultat dans qtrs
            // (accepté/rejeté). Pour distinguer proprement « Annuler » (bouton
            // ou touche Échap) d'un « OK », on vide le champ sur le signal
            // `rejected()` : après `exec()`, un champ vide == annulation.
            ffi::QObject_connect(
                dlg as *mut ffi::QObject,
                &sig_rejected,
                edit_ptr as *mut ffi::QObject,
                &slot_clear,
                0,
            );
            if !ok_conn || !cancel_conn {
                log(&format!(
                    "ERREUR: câblage des boutons du dialogue mot de passe (OK={} Annuler={})",
                    ok_conn, cancel_conn
                ));
            }
        }
    }

    // 4. Bloc modal : OK → accept(), Annuler → reject().
    unsafe { ffi::QDialog_exec(dlg); }

    // 5. Lecture de la valeur (dialogue et widgets encore en vie).
    let text = unsafe { ffi::QLineEdit_text(edit_ptr) };

    // 6. Nettoyage : on "oublie" le layout Rust pour éviter que son Drop ne
    //    supprime le QVBoxLayout C++ (déjà supprimé par le cascade
    //    QDialog_delete ci-dessous → double-free). Le petit leak de mémoire
    //    Rust (quelques octets par prompt) est négligeable.
    std::mem::forget(layout);
    unsafe { ffi::QDialog_delete(dlg); }

    if text.is_empty() {
        None
    } else {
        Some(text)
    }
}

/// Lance une commande avec sudo, en alimentant `stdin` avec le mot de passe.
///
/// `cmd` est la commande à exécuter SANS le `sudo` préfixe.
/// Retourne (code, sortie-stdout, sortie-stderr).
pub fn run_sudo(password: &str, cmd: &str, args: &[&str]) -> (i32, String, String) {
    let display = format!("{} {}", cmd, args.join(" "));
    let mut c = Command::new("sudo");
    c.arg("-S").arg(cmd).args(args).stdin(Stdio::piped());
    let mut child = match c.stdout(Stdio::piped()).stderr(Stdio::piped()).spawn() {
        Ok(child) => child,
        Err(e) => {
            log(&format!("ERREUR: impossible de lancer sudo {} : {}", display, e));
            return (-1, String::new(), e.to_string());
        }
    };
    // On écrit le mot de passe sur le stdin pipé, puis on ferme le tube.
    // (`sudo -S` attend le mot de passe sur stdin — un simple `c.output()` ne
    //  permet pas d'écrire dessus avant la lecture.)
    if let Some(mut stdin) = child.stdin.take() {
        use std::io::Write;
        let _ = stdin.write_all(password.as_bytes());
        let _ = stdin.write_all(b"\n");
    }
    let o = match child.wait_with_output() {
        Ok(o) => o,
        Err(e) => {
            log(&format!("ERREUR: lecture de la sortie de sudo {} : {}", display, e));
            return (-1, String::new(), e.to_string());
        }
    };
    let code = o.status.code().unwrap_or(-1);
    let stdout = String::from_utf8_lossy(&o.stdout).to_string();
    let stderr = String::from_utf8_lossy(&o.stderr).to_string();
    let status = if code == 0 { "OK".to_string() } else { format!("code {}", code) };
    log(&format!("$ sudo {} → {}", display, status));
    (code, stdout, stderr)
}

/// Lance une commande normale (sans sudo) et retourne (code, stdout, stderr).
#[allow(dead_code)]
pub fn run(cmd: &str, args: &[&str]) -> (i32, String, String) {
    let output = Command::new(cmd).args(args).output();
    match output {
        Ok(o) => (
            o.status.code().unwrap_or(-1),
            String::from_utf8_lossy(&o.stdout).to_string(),
            String::from_utf8_lossy(&o.stderr).to_string(),
        ),
        Err(e) => (-1, String::new(), e.to_string()),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn st(bsize: i64, bavail: i64, blocks: i64) -> Statfs {
        Statfs { f_bsize: bsize, f_bavail: bavail, f_blocks: blocks }
    }

    #[test]
    fn free_in_statfs_multiplies_bsize_by_bavail() {
        // 4096 o/bloc × 100 blocs = 409 600 octets libres.
        assert_eq!(free_in_statfs(&st(4096, 100, 200)), 409_600);
    }

    #[test]
    fn free_in_statfs_zero_when_no_blocks() {
        assert_eq!(free_in_statfs(&st(4096, 0, 200)), 0);
    }

    #[test]
    fn free_in_statfs_large_partition() {
        // 1 Go libres, blocs de 4 Ko : 262 144 blocs.
        assert_eq!(free_in_statfs(&st(4096, 262_144, 500_000)), 1_073_741_824);
    }

    #[test]
    fn total_in_statfs_multiplies_bsize_by_blocks() {
        assert_eq!(total_in_statfs(&st(4096, 100, 200)), 819_200);
        assert_eq!(total_in_statfs(&st(4096, 100, 0)), 0);
    }

    #[test]
    fn free_never_exceeds_total() {
        let s = st(4096, 100, 200);
        assert!(free_in_statfs(&s) <= total_in_statfs(&s));
    }

    #[test]
    fn boot_space_nopass_is_consistent() {
        // Sur une machine testée /boot peut être monté ou non : si la fonction
        // renvoie Some, les valeurs doivent être cohérentes.
        if let Some(sp) = boot_space_nopass() {
            assert!(sp.total > 0, "total > 0");
            assert!(sp.free <= sp.total, "libre ≤ total");
        }
    }

    #[test]
    fn walk_lsblk_finds_efi_partition() {
        let j: serde_json::Value = serde_json::from_str(
            r#"{"blockdevices":[{"name":"vda","path":"/dev/vda","fstype":null,
            "parttype":null,"mountpoint":null,"size":64424509440,
            "children":[
              {"name":"vda1","path":"/dev/vda1","fstype":"vfat",
               "parttype":"c12a7328-f81f-11d2-ba4b-00a0c93ec93b",
               "mountpoint":null,"size":2147483648},
              {"name":"vda2","path":"/dev/vda2","fstype":"btrfs",
               "parttype":"0fc63daf-8483-4772-8e79-3d69d8477de4",
               "mountpoint":"/","size":56074223616}
            ]}]}"#,
        )
        .unwrap();
        let mut out = Vec::new();
        for d in j.get("blockdevices").unwrap().as_array().unwrap() {
            walk_lsblk(d, &mut out);
        }
        assert_eq!(out.len(), 1, "seule l'ESP vfat doit être retenue");
        assert_eq!(out[0].0, "/dev/vda1");
        assert_eq!(out[0].1, 2_147_483_648);
        assert_eq!(out[0].2, None);
    }

    #[test]
    fn timeout_output_converts_seconds() {
        assert_eq!(format_timeout_output("Timeout value: 8 seconds"), "8 secondes");
        assert_eq!(format_timeout_output("Timeout value: 0 seconds\n"), "0 secondes");
        assert_eq!(format_timeout_output("Timeout value: 12 seconds"), "12 secondes");
    }

    #[test]
    fn timeout_output_handles_unset() {
        assert_eq!(
            format_timeout_output("No timeout is currently configured"),
            "non configuré (valeur par défaut)"
        );
    }

    #[test]
    fn timeout_output_handles_empty_and_unknown() {
        assert_eq!(format_timeout_output(""), "inconnu");
        assert_eq!(format_timeout_output("   "), "inconnu");
        assert_eq!(format_timeout_output("something else"), "something else");
    }

    #[test]
    fn kernel_suffix_recognized() {
        assert_eq!(kernel_suffix("7.3.0-4-cachyos"), "-cachyos");
        assert_eq!(kernel_suffix("7.2.11.zen1"), ".zen1");
        assert_eq!(kernel_suffix("7.2.12.lqx1-lqx"), "-lqx");
        assert_eq!(kernel_suffix("5.15.0-1"), "");
    }

    #[test]
    fn kernel_vendor_dirs() {
        assert_eq!(kernel_vendor_dir("7.3.0-4-cachyos"), "com.cachyos");
        assert_eq!(kernel_vendor_dir("7.2.11.zen1"), "com.zen-kernel");
        assert_eq!(kernel_vendor_dir("7.2.12.lqx1-lqx"), "com.liquorix");
        assert_eq!(kernel_vendor_dir("5.15.0-xanmod"), "com.external");
    }

    #[test]
    fn kernel_paths_for_version() {
        let kp = KernelPaths::for_version("7.3.0-4-cachyos");
        assert!(kp.vmlinuz.to_string_lossy().contains("com.cachyos/kernel-7.3.0-4-cachyos"));
        assert!(kp.initrd.to_string_lossy().contains("initrd-7.3.0-4-cachyos"));
        assert!(kp.boot_entry.to_string_lossy().ends_with("ext-7.3.0-4-cachyos.conf"));
        assert!(kp.modules.to_string_lossy().ends_with("/modules/7.3.0-4-cachyos"));
        let k2 = KernelPaths::for_version("7.2.12.lqx1-lqx");
        assert!(k2.vmlinuz.to_string_lossy().contains("com.liquorix"));
    }

    #[test]
    fn running_kernel_is_not_empty() {
        let r = running_kernel();
        assert!(!r.is_empty());
    }

    #[test]
    fn guess_kind_recognizes_modern_pacman_dirs() {
        use crate::repo::KernelKind;
        assert_eq!(guess_kind("7.3.0-4-cachyos"), Some(KernelKind::Cachyos));
        assert_eq!(guess_kind("7.2.11-zen1-1-zen"), Some(KernelKind::Zen));
        assert_eq!(guess_kind("7.2.11.zen1"), Some(KernelKind::Zen));
        assert_eq!(guess_kind("7.2.12-lqx1-1-lqx"), Some(KernelKind::Liquorix));
        assert_eq!(guess_kind("7.2.12.lqx1-lqx"), Some(KernelKind::Liquorix));
        assert_eq!(guess_kind("5.15.0-1"), None);
        assert_eq!(guess_kind("7.2.11-366.current"), None);
    }

    #[test]
    fn registry_roundtrip_in_tempdir() {
        use crate::repo::KernelKind;
        // On pointe le home vers un tmp pour isoler le registre.
        let tmp = std::env::temp_dir().join(format!("sekm-registry-test-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&tmp);
        std::fs::create_dir_all(&tmp).unwrap();
        std::env::set_var("HOME", &tmp);
        let reg = registry_path();
        assert!(!reg.exists());
        assert!(read_registry().is_empty());
        record_install("7.2.11-zen1-1-zen", KernelKind::Zen, Some("7.2.11.zen1"));
        assert_eq!(
            registry_kind("7.2.11-zen1-1-zen"),
            Some(KernelKind::Zen)
        );
        let r = read_registry();
        let e = r.get("7.2.11-zen1-1-zen").unwrap();
        assert_eq!(e.kind, "Zen");
        assert_eq!(e.display.as_deref(), Some("7.2.11.zen1"));
        forget_install("7.2.11-zen1-1-zen");
        assert!(read_registry().is_empty());
        let _ = std::fs::remove_dir_all(&tmp);
    }
}
