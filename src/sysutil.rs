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
pub fn installed_ext_kernels() -> Vec<String> {
    let mut out = Vec::new();
    if let Ok(dir) = fs::read_dir("/lib/modules") {
        for e in dir.flatten() {
            let name = e.file_name().to_string_lossy().to_string();
            if e.path().is_dir() && !kernel_suffix(&name).is_empty() {
                out.push(name);
            }
        }
    }
    out.sort();
    out
}

/// Sous-ensemble des noyaux installés partageant le même type (suffixe).
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
}
