//! Compilation d'un noyau depuis les sources de kernel.org, puis installation
//! par le chemin commun ([`crate::install::finish_install`]).
//!
//! Séquence d'un travail :
//!   1. pré-vol : outils de compilation, espace disque, cible déjà installée ;
//!   2. téléchargement + vérification PGP (fait par `source.rs`) ;
//!   3. extraction de l'archive ;
//!   4. `.config` de départ (`defconfig`, `localmodconfig` ou noyau courant) ;
//!   5. application des options (suffixe `LOCALVERSION`, préemption, `KCFLAGS`) ;
//!   6. `make -jN bzImage modules` (journal en direct, annulable) ;
//!   7. `sudo make modules_install` ;
//!   8. `depmod` + `dracut` + entrée systemd-boot (chemin partagé).
//!
//! Règles tenues ici :
//!  - la compilation se fait **en utilisateur normal** dans un répertoire de
//!    travail ; `sudo` n'est utilisé que pour `modules_install` et la fin
//!    d'installation ;
//!  - aucune progression en pourcentage pendant `make` : le pourcentage
//!    n'existe pas, donc on publie une étape + le nombre d'objets compilés +
//!    le temps écoulé ;
//!  - un `LOCALVERSION` non vide est **obligatoire** : un noyau vanilla de la
//!    même version que le noyau Solus écraserait ses modules sinon.

use std::io::{BufRead, BufReader, Write};
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc;
use std::sync::Arc;
use std::time::{Duration, Instant};

/// Espace disque exigé avant de lancer une compilation (sources + objets).
pub const MIN_FREE_BYTES: u64 = 12 * 1024 * 1024 * 1024;

/// Base du fichier `.config` avant application des options.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ConfigBase {
    /// Configuration du noyau en cours d'exécution (`/proc/config.gz` ou
    /// `/boot/config-$(uname -r)`).
    Current,
    /// `defconfig` puis `localmodconfig` : ne garde que le matériel détecté.
    LocalMod,
    /// `defconfig` complet fourni par les sources.
    Defconfig,
}

impl ConfigBase {
    pub fn label(self) -> &'static str {
        match self {
            ConfigBase::Current => "Config du noyau courant (recommandé)",
            ConfigBase::LocalMod => {
                "defconfig + localmodconfig (⚠ retire les pilotes intégrés : son, zram, FS…)"
            }
            ConfigBase::Defconfig => "defconfig complet",
        }
    }
    /// Identifiant accepté en ligne de commande.
    pub fn parse(s: &str) -> Option<Self> {
        match s.to_ascii_lowercase().as_str() {
            "current" => Some(ConfigBase::Current),
            "localmod" => Some(ConfigBase::LocalMod),
            "defconfig" => Some(ConfigBase::Defconfig),
            _ => None,
        }
    }
}

/// Modèle de préemption demandé.
///
/// `Keep` recopie le modèle du `.config` de départ : c'est le défaut, et le
/// bon choix quand `CONFIG_PREEMPT_DYNAMIC=y` (le modèle se change alors au
/// boot avec `preempt=…`, sans recompiler).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Preempt {
    Keep,
    None,
    Voluntary,
    Full,
    Lazy,
}

impl Preempt {
    pub fn label(self) -> &'static str {
        match self {
            Preempt::Keep => "Comme la config de départ",
            Preempt::None => "Aucune (serveur)",
            Preempt::Voluntary => "Volontaire (desktop)",
            Preempt::Full => "Basse latence",
            Preempt::Lazy => "Lazy (scheduler)",
        }
    }
    pub fn from_str(s: &str) -> Option<Self> {
        match s.to_ascii_lowercase().as_str() {
            "keep" => Some(Preempt::Keep),
            "none" => Some(Preempt::None),
            "voluntary" => Some(Preempt::Voluntary),
            "full" => Some(Preempt::Full),
            "lazy" => Some(Preempt::Lazy),
            _ => None,
        }
    }
    /// Identifiant accepté en ligne de commande.
    pub fn parse(s: &str) -> Option<Self> {
        Self::from_str(s)
    }
    /// Symbole attendu dans `.config` (None pour `Keep`).
    pub fn symbol(self) -> Option<&'static str> {
        match self {
            Preempt::Keep => None,
            Preempt::None => Some("PREEMPT_NONE"),
            Preempt::Voluntary => Some("PREEMPT_VOLUNTARY"),
            Preempt::Full => Some("PREEMPT"),
            Preempt::Lazy => Some("PREEMPT_LAZY"),
        }
    }
}

/// Options d'une compilation, telles que saisies dans l'onglet « Avancé ».
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BuildOptions {
    pub base: ConfigBase,
    pub preempt: Preempt,
    /// `KCFLAGS=-march=native` (gain réel ~1-3 %, binaire non portable).
    pub march_native: bool,
    /// Suffixe ajouté à `uname -r` (ex. `-kernelorg`). Jamais vide en pratique.
    pub localversion: String,
    /// Parallélisme de `make` (défaut : nombre de cœurs).
    pub jobs: u32,
    /// Répertoire de travail (archive, sources, objets).
    pub build_root: PathBuf,
}

impl Default for BuildOptions {
    fn default() -> Self {
        Self {
            // `defconfig` COMPLET par défaut : `localmodconfig` désactive tout
            // ce qui n'est pas un module chargé — or Solus compile en dur
            // btrfs, zram, le son, etc. Trois noyaux de suite ont été produits
            // sans racine montable, puis sans zram, puis sans audio.
            base: default_base(),
            preempt: Preempt::Keep,
            march_native: true,
            localversion: "-kernelorg".to_string(),
            jobs: default_jobs(),
            build_root: default_build_root(),
        }
    }
}

/// Nombre de cœurs disponibles (au moins 1).
pub fn default_jobs() -> u32 {
    std::thread::available_parallelism()
        .map(|n| n.get() as u32)
        .unwrap_or(1)
        .max(1)
}

/// Répertoire de travail par défaut : `~/.cache/…/kernel-build`.
pub fn default_build_root() -> PathBuf {
    let home = std::env::var("HOME").unwrap_or_else(|_| "/tmp".to_string());
    PathBuf::from(home).join(".cache/solus-external-kernel-manager/kernel-build")
}

/// Callbacks d'un travail : le worker du GUI les redirige vers la barre, le
/// statut et le journal ; les tests les capturent en mémoire.
pub trait BuildUi: Send {
    fn log(&self, line: &str);
    /// `pct` est une progression **par étapes** (0..100), jamais une mesure de
    /// `make`. `text` est le texte à afficher tel quel.
    fn status(&self, pct: i32, text: &str);
}

// ----------------------------------------------------------------------
// Pré-vol
// ----------------------------------------------------------------------

/// Résultat du pré-vol.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Preflight {
    /// Répertoire de travail examiné (affiché dans les messages d'erreur).
    pub root: PathBuf,
    /// Outils requis absents du `PATH`.
    pub missing: Vec<String>,
    /// Espace libre constaté (octets) dans le répertoire de travail.
    pub free: Option<u64>,
    /// Remarques non bloquantes (à afficher dans le journal).
    pub notes: Vec<String>,
}

impl Preflight {
    /// Message utilisateur (lisible) expliquant ce qui bloque, `None` si tout
    /// est en ordre.
    pub fn problem(&self) -> Option<String> {
        let mut parts: Vec<String> = Vec::new();
        if !self.missing.is_empty() {
            parts.push(format!(
                "outils manquants : {} — le bouton « Installer les outils de compilation » \
                 de l'onglet Avancé les installe (sinon : `sudo eopkg install -y system.devel \
                 bison flex m4 diffutils linux-headers bc perl dracut gnupg`)",
                self.missing.join(", ")
            ));
        }
        if let Some(free) = self.free {
            if free < MIN_FREE_BYTES {
                parts.push(format!(
                    "espace disque insuffisant : {} libres, {} requis pour les sources et les objets",
                    crate::fmt_bytes(free),
                    crate::fmt_bytes(MIN_FREE_BYTES)
                ));
            }
        } else {
            parts.push(format!(
                "espace disque du répertoire de travail illisible ({} et ses parents \
                 sont inaccessibles)",
                self.root.display()
            ));
        }
        if parts.is_empty() {
            None
        } else {
            Some(parts.join(" — "))
        }
    }
}

/// Paquets Solus de la chaîne de compilation complète, proposés en un clic
/// quand le pré-vol signale des outils manquants.
///
/// Cette liste a été établie en compilant réellement sur Solus : chaque entrée
/// correspond à un échec distinct et mal signalé par le noyau —
/// `system.devel` (gcc, make, binutils, glibc-devel), `bison`/`flex`/`m4`
/// (parseur de Kconfig, `m4` absent donne `exec of /usr/bin/m4 failed`),
/// `diffutils` (`cmp`, sans lui `Inconsistent kallsyms data` au lien),
/// `linux-headers` (`linux/limits.h`), `bc`, `perl`, `dracut` (initrd),
/// `gnupg` (signature PGP de l'archive).
pub const TOOLCHAIN_PACKAGES: &[&str] = &[
    // Pas de méta-paquet (`system.devel` n'existe pas dans tous les dépôts
    // Solus, et un seul nom inconnu fait échouer TOUTE la transaction eopkg).
    "gcc",
    "make",
    "binutils",
    "glibc-devel",
    "linux-headers",
    "bison",
    "flex",
    "m4",
    "diffutils",
    "elfutils-devel",
    "ncurses-devel",
    "bc",
    "perl",
    "dracut",
    "gnupg",
];

/// Vrai si `eopkg` est disponible (machine Solus).
pub fn eopkg_available() -> bool {
    crate::source::has_bin("eopkg")
}

/// Installe la chaîne de compilation avec `eopkg` (root requis).
///
/// Idempotent : `eopkg install` sur des paquets déjà présents ne fait rien.
/// Re-vérifie le pré-vol à la fin et échoue si un outil manque encore.
pub fn install_toolchain(
    password: &str,
    build_root: &Path,
    ui: &dyn BuildUi,
    cancel: &Arc<AtomicBool>,
) -> Result<(), String> {
    if !eopkg_available() {
        return Err(
            "`eopkg` est introuvable : cette fonction installe les paquets Solus. \
             Installez à la main l'équivalent de : gcc make binutils glibc-devel \
             linux-headers bison flex m4 diffutils elfutils-devel ncurses-devel bc perl \
             dracut gnupg."
                .to_string(),
        );
    }

    // `eopkg` refuse la transaction entière si UN nom est inconnu, et sort en
    // erreur quand tout est déjà installé. On filtre donc en amont : on ne
    // demande que les paquets présents dans le dépôt ET pas encore installés.
    ui.status(5, "Vérification des paquets disponibles…");
    let root = Path::new("/");
    let mut to_install: Vec<&str> = Vec::new();
    for pkg in TOOLCHAIN_PACKAGES {
        let avail = run_in(root, "eopkg", &["info", pkg])
            .map(|(code, _o, _e)| code == 0)
            .unwrap_or(false);
        if !avail {
            ui.log(&format!("  {} : absent du dépôt, ignoré", pkg));
            continue;
        }
        let installed = run_in(root, "eopkg", &["li", pkg])
            .map(|(code, out, _e)| code == 0 && !out.to_ascii_lowercase().contains("not installed"))
            .unwrap_or(false);
        if installed {
            continue;
        }
        to_install.push(pkg);
    }

    if to_install.is_empty() {
        ui.log("Tous les paquets de compilation sont déjà installés.");
    } else {
        let mut args: Vec<&str> = vec!["-S", "eopkg", "install", "-y"];
        args.extend_from_slice(&to_install);
        ui.log(&format!(
            "Installation des outils de compilation : sudo eopkg install -y {}",
            to_install.join(" ")
        ));
        ui.status(15, "Installation des outils de compilation (eopkg)…");
        run_streaming(root, "sudo", &args, &[], Some(password), ui, cancel, false)?;
    }

    ui.status(95, "Vérification des outils installés…");
    let pf = preflight(build_root, true);
    if !pf.missing.is_empty() {
        return Err(format!(
            "après installation, il manque encore : {}",
            pf.missing.join(", ")
        ));
    }
    ui.status(100, "Outils de compilation installés ✓");
    ui.log("Tous les outils de compilation sont présents.");
    Ok(())
}

/// Désinstalle les paquets de [`TOOLCHAIN_PACKAGES`] qui sont installés.
///
/// `eopkg` refuse de retirer un paquet dont dépend un autre paquet installé :
/// c'est la protection voulue, et l'échec est rapporté sans être masqué.
pub fn uninstall_toolchain(
    password: &str,
    ui: &dyn BuildUi,
    cancel: &Arc<AtomicBool>,
) -> Result<(), String> {
    if !eopkg_available() {
        return Err("`eopkg` est introuvable : rien à désinstaller avec cet outil.".to_string());
    }
    let root = Path::new("/");
    let mut to_remove: Vec<&str> = Vec::new();
    for pkg in TOOLCHAIN_PACKAGES {
        let installed = run_in(root, "eopkg", &["li", pkg])
            .map(|(code, out, _e)| code == 0 && !out.to_ascii_lowercase().contains("not installed"))
            .unwrap_or(false);
        if installed {
            to_remove.push(pkg);
        }
    }
    if to_remove.is_empty() {
        ui.log("Aucun paquet de compilation à désinstaller.");
        ui.status(100, "Rien à désinstaller.");
        return Ok(());
    }
    let mut args: Vec<&str> = vec!["-S", "eopkg", "remove", "-y"];
    args.extend_from_slice(&to_remove);
    ui.log(&format!(
        "Désinstallation : sudo eopkg remove -y {}",
        to_remove.join(" ")
    ));
    ui.status(20, "Désinstallation des outils de compilation…");
    // Un paquet requis par un autre est refusé par eopkg : on continue, puis on
    // liste ce qui reste.
    let _ = run_streaming(root, "sudo", &args, &[], Some(password), ui, cancel, false);
    let mut restants: Vec<&str> = Vec::new();
    for pkg in &to_remove {
        let still = run_in(root, "eopkg", &["li", pkg])
            .map(|(code, out, _e)| code == 0 && !out.to_ascii_lowercase().contains("not installed"))
            .unwrap_or(false);
        if still {
            restants.push(pkg);
        }
    }
    if restants.is_empty() {
        ui.log("Tous les paquets de compilation ont été retirés.");
    } else {
        ui.log(&format!(
            "Conservés car requis par d'autres paquets Solus (refus normal d'eopkg) : {}",
            restants.join(", ")
        ));
    }
    ui.status(100, "Désinstallation terminée.");
    Ok(())
}

/// Outils indispensables à la compilation (nom du binaire dans le `PATH`).
/// `m4` fait partie de la liste : `flex` et `bison` l'exécutent au moment de
/// générer les sources du parseur de Kconfig. Sans lui, `make defconfig`
/// échoue avec `flex : erreur interne fatale, exec of /usr/bin/m4 failed`
/// (code 141 = SIGPIPE), un message qui n'aide pas à trouver le paquet manquant.
/// `cmp` (paquet `diffutils`) fait aussi partie de la liste : `make modules`
/// l'utilise pour comparer les passes de `kallsyms`, et `lstopo`/`sorttable`
/// n'est pas le seul à en dépendre. Sans lui, le lien de `vmlinux` échoue avec
/// « Inconsistent kallsyms data » — un message qui n'oriente pas du tout vers
/// le paquet manquant.
pub const REQUIRED_TOOLS: &[&str] = &[
    "gcc", "make", "ld", "bison", "flex", "m4", "cmp", "bc", "perl", "tar", "xz", "dracut",
    "depmod",
];

/// Vérifie les prérequis d'une compilation dans `build_root`.
pub fn preflight(build_root: &Path, march_native: bool) -> Preflight {
    let mut missing: Vec<String> = Vec::new();
    for t in REQUIRED_TOOLS {
        if !crate::source::has_bin(t) {
            missing.push((*t).to_string());
        }
    }
    // `gpg` est requis pour vérifier la signature : sans lui, pas de build.
    if !crate::source::has_bin("gpg") {
        missing.push("gpg".to_string());
    }
    let mut notes: Vec<String> = Vec::new();
    // Une compilation d'essai, pas seulement la présence de `gcc` : sur Solus,
    // `gcc` s'installe sans les en-têtes ni les fichiers de démarrage C
    // (`glibc-devel`), et `make defconfig` échoue alors des minutes plus tard
    // sur `scripts/basic/fixdep` avec « cannot find Scrt1.o ». Le pré-vol doit
    // voir ça tout de suite.
    if !c_compiler_works() {
        missing.push("en-têtes C de compilation (`glibc-devel`, `linux-headers`)".to_string());
    }
    // Sonde séparée : `gelf.h` vient d'`elfutils-devel`, pas de glibc-devel.
    // Sans elle, le pré-vol passait et la compilation échouait à `objtool`.
    if !header_available("gelf.h") {
        missing.push("`gelf.h` (paquet `elfutils-devel`, requis par objtool)".to_string());
    }
    if march_native && !gcc_accepts_march_native() {
        notes.push(
            "-march=native ignoré : le compilateur installé ne reconnaît pas cette option"
                .to_string(),
        );
    }
    // Le répertoire de travail n'existe pas encore au premier lancement (et
    // pas davantage son parent `~/.cache/solus-external-kernel-manager`) :
    // `statvfs` y échoue et l'utilisateur recevait « espace disque du
    // répertoire de travail illisible » sur un disque parfaitement sain.
    // On crée le répertoire, puis on remonte au premier ancêtre existant.
    let _ = std::fs::create_dir_all(build_root);
    let free = free_bytes_nearest(build_root);
    Preflight {
        root: build_root.to_path_buf(),
        missing,
        free,
        notes,
    }
}

/// Espace libre du premier ancêtre **existant** de `path` (ou de `path`
/// lui-même une fois créé). `statvfs` échoue sur un chemin absent : sans cette
/// remontée, un répertoire de travail neuf fait croire à un disque illisible.
pub fn free_bytes_nearest(path: &Path) -> Option<u64> {
    let mut cur: Option<&Path> = Some(path);
    while let Some(p) = cur {
        if let Some(b) = crate::sysutil::free_bytes_at(&p.to_string_lossy()) {
            return Some(b);
        }
        cur = p.parent();
    }
    None
}

/// Compile et lie un programme C minimal : prouve que `gcc` **et** les
/// en-têtes/fichiers de démarrage de la libc sont installés.
fn c_compiler_works() -> bool {
    let probe = std::env::temp_dir().join(format!("sekm-cc-probe-{}.c", std::process::id()));
    let out = std::env::temp_dir().join(format!("sekm-cc-probe-{}", std::process::id()));
    // `linux/limits.h` (paquet `linux-headers` sur Solus) est inclus par
    // `bits/local_lim.h` : sans lui, `make defconfig` échoue sur
    // `scripts/kconfig/conf.o` alors que `gcc` compile un programme simple.
    let src = "#include <stdio.h>\n#include <linux/limits.h>\nint main(void){return 0;}\n";
    if std::fs::write(&probe, src).is_err() {
        return false;
    }
    let ok = Command::new("gcc")
        .arg(&probe)
        .arg("-o")
        .arg(&out)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status()
        .map(|s| s.success())
        .unwrap_or(false);
    let _ = std::fs::remove_file(&probe);
    let _ = std::fs::remove_file(&out);
    ok
}

/// Vrai si un en-tête système est installé (compilation d'essai minimale).
fn header_available(header: &str) -> bool {
    let probe = std::env::temp_dir().join(format!(
        "sekm-hdr-{}-{}.c",
        header.replace(['/', '.'], "_"),
        std::process::id()
    ));
    if std::fs::write(&probe, format!("#include <{}>\nint main(void){{return 0;}}\n", header))
        .is_err()
    {
        return false;
    }
    let ok = Command::new("gcc")
        .arg("-fsyntax-only")
        .arg(&probe)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status()
        .map(|s| s.success())
        .unwrap_or(false);
    let _ = std::fs::remove_file(&probe);
    ok
}

/// Vrai si `gcc` accepte `-march=native` (préprocesseur vide, aucun fichier).
fn gcc_accepts_march_native() -> bool {
    Command::new("gcc")
        .args(["-march=native", "-E", "-"])
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status()
        .map(|s| s.success())
        .unwrap_or(false)
}

// ----------------------------------------------------------------------
// Sources
// ----------------------------------------------------------------------

/// Répertoire de travail d'une version (`<build_root>/linux-<version>`).
pub fn work_dir(build_root: &Path, version: &str) -> PathBuf {
    build_root.join(format!("linux-{}", version))
}

/// Déduit le nom du répertoire source depuis le nom de l'archive.
///
/// `linux-6.12.118.tar.xz` → `linux-6.12.118`.
pub fn source_dir_name(tarball_name: &str) -> Option<String> {
    let base = tarball_name
        .strip_suffix(".tar.xz")
        .or_else(|| tarball_name.strip_suffix(".tar.gz"))
        .or_else(|| tarball_name.strip_suffix(".tar.bz2"))?;
    if base.is_empty() || base.contains('/') {
        return None;
    }
    Some(base.to_string())
}

/// Extrait l'archive (`tar -xJf`) dans le répertoire de travail.
/// Retourne le répertoire des sources.
pub fn extract(tarball: &Path, work: &Path, ui: &dyn BuildUi) -> Result<PathBuf, String> {
    let name = tarball
        .file_name()
        .map(|s| s.to_string_lossy().to_string())
        .ok_or("nom d'archive illisible")?;
    let dir_name = source_dir_name(&name)
        .ok_or_else(|| format!("nom d'archive inattendu : {}", name))?;
    let src = work.join(&dir_name);
    if src.join("Makefile").exists() {
        ui.log(&format!(
            "Sources déjà extraites : {} (extraction évitée)",
            src.display()
        ));
        return Ok(src);
    }
    std::fs::create_dir_all(work)
        .map_err(|e| format!("création de {} impossible : {}", work.display(), e))?;
    ui.log(&format!("Extraction de {} …", name));
    // `-xf` : GNU tar détecte seul la compression (xz, gzip, bzip2). `-xJf`
    // imposait xz et cassait sur une archive `.tar.gz` (cas des RC).
    let (code, _out, err) = run(
        "tar",
        &[
            "-xf",
            &tarball.to_string_lossy(),
            "-C",
            &work.to_string_lossy(),
        ],
    );
    if code != 0 {
        return Err(format!(
            "extraction impossible (code {}) : {}",
            code,
            tail(&err)
        ));
    }
    if !src.join("Makefile").exists() {
        return Err(format!(
            "l'archive n'a pas produit {} (Makefile absent)",
            src.display()
        ));
    }
    Ok(src)
}

// ----------------------------------------------------------------------
// Configuration
// ----------------------------------------------------------------------

/// Compte rendu de la préparation du `.config`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ConfigReport {
    /// `make -s kernelrelease` = futur `uname -r`.
    pub kver: String,
    /// Modèle de préemption réellement actif dans `.config`.
    pub preempt_active: String,
    /// Symboles demandés mais indisponibles dans cette version.
    pub ignored: Vec<String>,
    /// Le modèle est-il modifiable au boot (`CONFIG_PREEMPT_DYNAMIC=y`) ?
    pub preempt_dynamic: bool,
    /// Nombre de modules (`=m`) du `.config` final.
    pub modules: usize,
}

/// Prépare le `.config` : base, suffixe de version, préemption, puis
/// `olddefconfig` (qui résout les dépendances entre symboles).
pub fn prepare_config(
    src: &Path,
    opts: &BuildOptions,
    ui: &dyn BuildUi,
) -> Result<ConfigReport, String> {
    // --- 1. Base ---------------------------------------------------------
    match opts.base {
        ConfigBase::Current => {
            ui.log("Base : configuration du noyau en cours d'exécution");
            copy_current_config(src)?;
        }
        ConfigBase::Defconfig => {
            ui.log("Base : defconfig des sources");
            make_quiet(src, &["defconfig"], ui)?;
        }
        ConfigBase::LocalMod => {
            ui.log("Base : defconfig + localmodconfig (modules du matériel détecté)");
            make_quiet(src, &["defconfig"], ui)?;
            // `localmodconfig` pose des questions ; stdin fermé = valeurs par
            // défaut, ce qui est le comportement attendu ici.
            make_quiet(src, &["localmodconfig"], ui)?;
            ui.log(
                "ATTENTION : localmodconfig ne garde que ce qui est chargé MAINTENANT. Il \
                 supprime aussi les pilotes que Solus compile en dur (btrfs, zram, son, carte \
                 réseau) : le noyau peut ne pas monter sa racine, perdre le son et attendre \
                 30 s par périphérique absent. Préférez « defconfig complet ».",
            );
        }
    }

    // --- 1b. Système de fichiers racine ----------------------------------
    // `localmodconfig` désactive tout module qui n'est pas chargé — y compris
    // un pilier compilé EN DUR dans le noyau de la distribution, qui n'apparaît
    // jamais dans /proc/modules. Sur Solus, btrfs est `=y` dans le noyau
    // courant et defconfig le met en `=m` : `localmodconfig` le SUPPRIME, et le
    // noyau obtenu ne peut plus monter sa racine (panic avant userspace, donc
    // aucun journal — exactement le cas rencontré sur le PC de test).
    if let Some(fs) = root_fstype() {
        if let Some(sym) = fs_symbol_for(&fs) {
            if !config_has(src, sym) {
                let _ = scripts_config(src, &["--enable", sym]);
                ui.log(&format!(
                    "Racine {} : CONFIG_{}=y réactivé (la base de .config l'avait désactivé — \
                     le noyau n'aurait pas pu monter sa racine)",
                    fs, sym
                ));
            } else {
                ui.log(&format!("Racine {} : CONFIG_{} déjà actif", fs, sym));
            }
        } else {
            ui.log(&format!(
                "Racine {} : type inconnu, aucun garde-fou appliqué (vérifiez que le pilote \
                 est dans le noyau)",
                fs
            ));
        }
    }

    // --- 1c. Passe de complétude ------------------------------------------
    // `defconfig` n'est PAS un noyau complet : mesuré 13 modules sur 6.12,
    // `# CONFIG_ZRAM is not set`, aucun codec audio. On réactive donc les
    // sous-systèmes qu'un bureau utilise, en modules, quel que soit le point de
    // départ (localmodconfig les supprimait de la même façon).
    let mut enabled_essentials = enable_essentials(src);
    ui.log(&format!(
        "Passe de complétude : {} sous-système(s) réactivé(s) (zram, audio, pare-feu, \
         systèmes de fichiers, matériel courant)",
        enabled_essentials
    ));

    // --- 2. Suffixe de version -------------------------------------------
    validate_localversion(&opts.localversion)?;
    let lv = opts.localversion.trim();
    ui.log(&format!("LOCALVERSION = {}", lv));
    scripts_config(src, &["--set-str", "LOCALVERSION", lv])?;

    // --- 3. Préemption ----------------------------------------------------
    // Les modèles sont mutuellement exclusifs : on active celui demandé et on
    // désactive les autres. `scripts/config` écrit la ligne même si le symbole
    // n'existe pas dans le Kconfig de cette version ; `olddefconfig` la
    // supprime ensuite. On vérifie donc d'abord l'EXISTENCE (formes
    // `CONFIG_X=y` et `# CONFIG_X is not set`), puis le résultat final.
    let mut ignored: Vec<String> = Vec::new();
    let mut requested: Option<&'static str> = None;
    if let Some(sym) = opts.preempt.symbol() {
        if !config_symbol_exists(src, sym) {
            // Ne rien désactiver : sur cette version le symbole n'existe pas,
            // et éteindre les autres modèles laisserait le .config sans choix.
            ui.log(&format!(
                "{} n'existe pas dans cette version du noyau : modèle de départ conservé",
                sym
            ));
            ignored.push(format!("{} (absent de cette version du noyau)", sym));
        } else {
            requested = Some(sym);
            for other in [
                "PREEMPT_NONE",
                "PREEMPT_VOLUNTARY",
                "PREEMPT",
                "PREEMPT_LAZY",
                "PREEMPT_RT",
            ] {
                if other != sym {
                    let _ = scripts_config(src, &["--disable", other]);
                }
            }
            if let Err(e) = scripts_config(src, &["--enable", sym]) {
                ignored.push(format!("{} ({})", sym, e));
                requested = None;
            } else {
                ui.log(&format!("Préemption demandée : {}", sym));
            }
        }
    }

    // --- 3b. BTF : `pahole` obligatoire ----------------------------------
    // `CONFIG_DEBUG_INFO_BTF=y` (défaut de defconfig) fait appel à `pahole`
    // pour produire `vmlinux.h` à la fin du lien : sans lui, la compilation
    // échoue APRÈS des heures, sur les dernières étapes. On le désactive ici et
    // on le dit, plutôt que de laisser découvrir l'erreur au bout du compte.
    if config_symbol_exists(src, "DEBUG_INFO_BTF") && !crate::source::has_bin("pahole") {
        let _ = scripts_config(src, &["--disable", "DEBUG_INFO_BTF"]);
        ui.log(
            "pahole absent : CONFIG_DEBUG_INFO_BTF désactivé — BPF CO-RE et le profilage \
             symbolique ne seront pas disponibles sur ce noyau (le reste est inchangé).",
        );
    }

    // --- 4. Résolution des dépendances ------------------------------------
    make_quiet(src, &["olddefconfig"], ui)?;

    // 4b. Seconde passe : Kconfig a pu effacer un symbole dont la dépendance
    // n'était pas encore résolue au moment de l'écriture (NF_TABLES_INET et
    // NFT_CT dans le cas réel). On réessaie une fois, puis on re-résout.
    let retried = enable_essentials(src);
    if retried > 0 {
        enabled_essentials += retried;
        ui.log(&format!(
            "Passe de complétude (2e) : {} sous-système(s) rattrapé(s) après résolution \
             ({} au total)",
            retried, enabled_essentials
        ));
        make_quiet(src, &["olddefconfig"], ui)?;
    }

    // --- 5. Relecture de ce qui est réellement actif ----------------------
    let kver = make_kernelrelease(src)?;
    let preempt_active = active_preempt(src).unwrap_or_else(|| "inconnu".to_string());
    let preempt_dynamic = config_has(src, "PREEMPT_DYNAMIC");
    // Garde-fou final : un noyau sans le pilote de sa propre racine ne démarre
    // pas. On refuse avant les heures de compilation plutôt que de produire un
    // noyau qui panic au montage de `/`.
    if let Some(sym) = root_fstype().as_deref().and_then(fs_symbol_for) {
        if !config_has(src, sym) {
            return Err(format!(
                "le .config final n'active pas CONFIG_{} : ce noyau ne pourrait pas monter \
                 sa racine. Compilation refusée (choisissez une autre base de .config, ou \
                 activez-le dans menuconfig).",
                sym
            ));
        }
    }
    if let Some(want) = requested {
        if preempt_active != want {
            // Le symbole existait mais n'est pas resté actif (dépendance non
            // satisfaite) : on le signale au lieu de laisser croire au succès.
            ignored.push(format!(
                "{} (le .config final contient {})",
                want, preempt_active
            ));
            ui.log(&format!(
                "{} n'a pas pu être activé (dépendance) : le .config final contient {}",
                want, preempt_active
            ));
        }
    }
    if preempt_dynamic {
        ui.log(
            "CONFIG_PREEMPT_DYNAMIC=y : le modèle de préemption peut aussi se changer au boot \
             (`preempt=lazy|voluntary|full|none` sur la ligne de commande du noyau), sans recompiler.",
        );
    }
    // Les essentiels demandés et absents du .config final (dépendance non
    // satisfaite) sont signalés : c'est ce qui produirait un noyau muet ou
    // attendant zram pendant 30 s.
    let mut missing_essentials: Vec<&str> = Vec::new();
    let mut missing_fatal: Vec<&str> = Vec::new();
    for sym in ESSENTIAL_MODULES.iter().chain(ESSENTIAL_BUILTIN.iter()) {
        if config_symbol_exists(src, sym) && !config_has(src, sym) {
            ignored.push(format!("{} (désactivé par une dépendance)", sym));
            if ESSENTIAL_FATAL.contains(sym) {
                missing_fatal.push(sym);
            } else {
                missing_essentials.push(sym);
            }
        }
    }
    if !missing_essentials.is_empty() {
        ui.log(&format!(
            "Essentiels non activables sur ce noyau (dépendance absente, compilation \
             poursuivie) : {}",
            missing_essentials.join(", ")
        ));
    }
    let modules = std::fs::read_to_string(src.join(".config"))
        .unwrap_or_default()
        .lines()
        .filter(|l| l.ends_with("=m"))
        .count();
    ui.log(&format!(
        "Configuration prête — uname -r prévu : {} — préemption : {} — {} modules",
        kver, preempt_active, modules
    ));
    // Garde-fou : si un sous-système essentiel existait dans ce noyau mais que
    // le `.config` final le désactive, le noyau sera muet ou attendra 30 s par
    // périphérique absent. On refuse — c'est exactement le cas « 13 modules,
    // zram not set » qui a été livré trois fois de suite.
    if !missing_fatal.is_empty() {
        return Err(format!(
            "ces sous-systèmes indispensables sont restés désactivés dans le .config final : \
             {}. Sans eux le noyau n'a pas de son ou attend 30 s par périphérique absent. \
             Compilation refusée — activez-les via « 2. Ouvrir menuconfig ».",
            missing_fatal.join(", ")
        ));
    }

    Ok(ConfigReport {
        kver,
        preempt_active,
        ignored,
        preempt_dynamic,
        modules,
    })
}

/// Copie la configuration du noyau courant dans `<src>/.config`.
///
/// Sources essayées dans l'ordre : `/proc/config.gz`, `/boot/config-<uname -r>`,
/// `/usr/lib/modules/<uname -r>/config`. Aucune n'est garantie (Solus ne
/// compile pas forcément `CONFIG_IKCONFIG_PROC`) : dans ce cas on explique
/// quoi choisir à la place.
pub fn copy_current_config(src: &Path) -> Result<(), String> {
    let running = crate::sysutil::running_kernel();
    let dest = src.join(".config");

    if Path::new("/proc/config.gz").exists() {
        let out = Command::new("sh")
            .arg("-c")
            .arg("zcat /proc/config.gz")
            .output()
            .map_err(|e| format!("zcat /proc/config.gz : {}", e))?;
        if out.status.success() && out.stdout.len() > 1024 {
            std::fs::write(&dest, &out.stdout).map_err(|e| e.to_string())?;
            return Ok(());
        }
    }
    for cand in [
        format!("/boot/config-{}", running),
        format!("/usr/lib/modules/{}/config", running),
        format!("/lib/modules/{}/config", running),
    ] {
        if let Ok(data) = std::fs::read(&cand) {
            if data.len() > 1024 {
                std::fs::write(&dest, &data).map_err(|e| e.to_string())?;
                return Ok(());
            }
        }
    }
    Err(format!(
        "configuration du noyau courant introuvable (essayé : /proc/config.gz, /boot/config-{}, \
         /usr/lib/modules/{}/config). Ce noyau n'expose pas sa configuration : choisissez \
         « defconfig + localmodconfig » ou « defconfig complet ».",
        running, running
    ))
}

/// Sous-systèmes qu'un poste de travail attend, et que `x86_64_defconfig`
/// désactive ou ignore (mesuré sur 6.12 et 7.3 : 13 modules seulement, zram et
/// les codecs audio explicitement « not set »). Activés en **modules** au-dessus
/// de la base choisie, quelle qu'elle soit — y compris `localmodconfig`, qui les
/// supprimait aussi.
pub const ESSENTIAL_MODULES: &[&str] = &[
    // mémoire compressée : `zram-generator` de Solus en dépend, et sans elle
    // chaque périphérique attendu coûte 30 s de timeout au démarrage.
    "ZRAM",
    // audio (le contrôleur HDA est en dur dans defconfig, les codecs non)
    "SND",
    "SND_HDA_INTEL",
    // Depuis ~6.5, `SND_HDA_CODEC_REALTEK` n'est plus qu'un noyau commun : les
    // codecs sont des modules PAR PUCES. Un portable Dell/HP typique (ALC3234,
    // ALC236, ALC295…) a donc besoin de `SND_HDA_CODEC_ALC269` ou `ALC662`
    // explicitement — sinon le contrôleur est là, la carte apparaît dans
    // /proc/asound/cards, et aucun son ne sort (cas réel : ALC3234).
    "SND_HDA_CODEC_REALTEK",
    "SND_HDA_CODEC_REALTEK_LIB",
    "SND_HDA_CODEC_ALC269",
    "SND_HDA_CODEC_ALC260",
    "SND_HDA_CODEC_ALC262",
    "SND_HDA_CODEC_ALC268",
    "SND_HDA_CODEC_ALC662",
    "SND_HDA_CODEC_ALC663",
    "SND_HDA_CODEC_ALC882",
    "SND_HDA_CODEC_ALC883",
    "SND_HDA_CODEC_ALC861",
    "SND_HDA_CODEC_HDMI",
    "SND_HDA_CODEC_ANALOG",
    "SND_HDA_CODEC_CONEXANT",
    "SND_HDA_CODEC_VIA",
    "SND_HDA_CODEC_IDT",
    "SND_HDA_CODEC_SIGMATEL",
    "SND_HDA_CODEC_CIRRUS",
    "SND_USB_AUDIO",
    // pare-feu / réseau
    "NF_TABLES",
    // `NF_TABLES_INET` ne produit aucun module séparé sur 6.12 (la famille
    // inet est dans nf_tables) : inutile de le réclamer.
    "NFT_CT",
    "NFT_NAT",
    "IP_NF_IPTABLES",
    "IP_NF_FILTER",
    "IP_NF_NAT",
    "WIREGUARD",
    // systèmes de fichiers et conteneurs
    "FUSE_FS",
    "OVERLAY_FS",
    "EXFAT_FS",
    "NTFS3_FS",
    "SQUASHFS",
    "CIFS",
    "DM_CRYPT",
    // matériel courant des portables et des fixes.
    // ATTENTION : kconfig n'émet AUCUNE ligne pour un symbole dont les
    // dépendances sont désactivées — d'où les parents (`BT`, `MEDIA_SUPPORT`,
    // `MEDIA_USB_SUPPORT`, `HWMON`) listés AVANT leurs enfants. Et les noms
    // changent : `CORETEMP` est devenu `SENSORS_CORETEMP`, `UVCVIDEO` est
    // `USB_VIDEO_CLASS` (vérifié sur 6.12 en inspectant le noyau produit).
    "BT",
    "BT_HCIBTUSB",
    "BT_USB",
    "MEDIA_SUPPORT",
    "USB_VIDEO_CLASS",
    "SENSORS_CORETEMP",
    "IWLWIFI",
    "I2C_CHARDEV",
];

/// Symboles dont l'absence casse le démarrage ou le son : leur absence après
/// `olddefconfig` est ÉLIMINATOIRE. Les autres essentiels ne produisent qu'un
/// avertissement : `NF_TABLES_INET` ou `NFT_CT` peuvent légitimement rester
/// éteints si une dépendance du noyau choisi manque, et refuser la compilation
/// pour eux bloquait un noyau par ailleurs correct.
pub const ESSENTIAL_FATAL: &[&str] = &[
    "ZRAM",
    "SND_HDA_INTEL",
    "SND_HDA_CODEC_REALTEK",
    "SND_HDA_CODEC_HDMI",
];

/// Active tous les essentiels encore désactivés. Retourne le nombre de
/// symboles effectivement activés. À appeler deux fois : une première passe
/// avant `olddefconfig`, puis une seconde APRÈS, car Kconfig efface un symbole
/// dont la dépendance n'était pas encore résolue (c'est ce qui faisait échouer
/// `NF_TABLES_INET` et `NFT_CT`).
fn enable_essentials(src: &Path) -> usize {
    let mut n = 0usize;
    for sym in ESSENTIAL_MODULES {
        if config_has(src, sym) || !config_symbol_exists(src, sym) {
            continue;
        }
        if enable_module(src, sym).is_ok() {
            n += 1;
        }
    }
    for sym in ESSENTIAL_BUILTIN {
        if config_has(src, sym) || !config_symbol_exists(src, sym) {
            continue;
        }
        if enable_builtin(src, sym).is_ok() {
            n += 1;
        }
    }
    n
}

/// Symboles booléens (pas des tristates) à activer en dur : `--module` écrirait
/// `=m` sur un booléen, ce que kconfig signale comme invalide.
/// Symboles **booléens** (pas des tristates) : `--module` écrirait `=m`, que
/// kconfig rejette pour un booléen — c'est ce qui faisait échouer silencieusement
/// `MEDIA_CAMERA_SUPPORT`/`MEDIA_USB_SUPPORT` (donc `uvcvideo`), `HWMON` et
/// `IWLWIFI_LEDS`.
pub const ESSENTIAL_BUILTIN: &[&str] = &[
    "X86_INTEL_PSTATE",
    "ACPI_THERMAL",
    "HWMON",
    "MEDIA_CAMERA_SUPPORT",
    "MEDIA_USB_SUPPORT",
    "IWLWIFI_LEDS",
];

/// Active un symbole en module (`=m`).
fn enable_module(src: &Path, symbol: &str) -> Result<(), String> {
    scripts_config(src, &["--module", symbol])
}

/// Active un symbole booléen (`=y`).
fn enable_builtin(src: &Path, symbol: &str) -> Result<(), String> {
    scripts_config(src, &["--enable", symbol])
}

/// Symbole Kconfig du système de fichiers, d'après son nom de type.
pub fn fs_symbol_for(fstype: &str) -> Option<&'static str> {
    match fstype.to_ascii_lowercase().as_str() {
        "btrfs" => Some("BTRFS_FS"),
        "ext4" | "ext3" | "ext2" => Some("EXT4_FS"),
        "xfs" => Some("XFS_FS"),
        "f2fs" => Some("F2FS_FS"),
        "vfat" | "fat" | "msdos" => Some("VFAT_FS"),
        "ntfs3" => Some("NTFS3_FS"),
        _ => None,
    }
}

/// Vrai si le système expose la configuration du noyau en cours
/// (`/proc/config.gz` ou `/boot/config-<uname -r>`). C'est de loin la
/// meilleure base : elle contient exactement les pilotes qui font marcher la
/// machine (mesuré sur le PC de test : 6544 modules contre 84 pour un
/// defconfig enrichi, et le seul noyau compilé qui avait du son).
pub fn current_config_available() -> bool {
    if Path::new("/proc/config.gz").exists() {
        return true;
    }
    let running = crate::sysutil::running_kernel();
    [
        format!("/boot/config-{}", running),
        format!("/usr/lib/modules/{}/config", running),
        format!("/lib/modules/{}/config", running),
    ]
    .iter()
    .any(|p| Path::new(p).exists())
}

/// Base par défaut : la config du noyau courant si elle est disponible, sinon
/// `defconfig complet`.
pub fn default_base() -> ConfigBase {
    if current_config_available() {
        ConfigBase::Current
    } else {
        ConfigBase::Defconfig
    }
}

/// Type du système de fichiers qui porte `/`, lu dans `/proc/mounts`.
pub fn root_fstype() -> Option<String> {
    let mounts = std::fs::read_to_string("/proc/mounts").ok()?;
    mounts.lines().find_map(|line| {
        let mut it = line.split_whitespace();
        let _dev = it.next()?;
        let mp = it.next()?;
        let fs = it.next()?;
        if mp == "/" {
            Some(fs.to_string())
        } else {
            None
        }
    })
}

/// Contrôle du suffixe `LOCALVERSION`.
///
/// Il finit en nom de répertoire sous `/lib/modules` et en argument de
/// commande : vide ou exotique, il casse l'installation ou écrase le noyau de
/// la distribution. Refus explicite plutôt que correction silencieuse.
pub fn validate_localversion(lv: &str) -> Result<(), String> {
    let t = lv.trim();
    if t.is_empty() {
        return Err(
            "le suffixe de version (LOCALVERSION) ne peut pas être vide : un noyau vanilla \
             de la même version que celui de Solus écraserait ses modules. Utilisez par \
             exemple « -kernelorg »."
                .to_string(),
        );
    }
    if !t
        .chars()
        .all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_' || c == '.')
    {
        return Err(format!(
            "suffixe de version invalide « {} » (lettres, chiffres, `-`, `_`, `.` uniquement)",
            t
        ));
    }
    Ok(())
}

/// Vrai si le symbole **existe** dans le Kconfig de cette version, actif ou
/// non. Un `.config` généré liste tous les booléens connus, soit
/// `CONFIG_X=y`, soit `# CONFIG_X is not set` : l'absence des deux formes
/// signifie que le symbole n'existe pas dans cette version du noyau (cas de
/// `PREEMPT_LAZY` avant 6.13). Sert à ne rien casser quand on demande un
/// modèle de préemption indisponible.
pub fn config_symbol_exists(src: &Path, symbol: &str) -> bool {
    let cfg = std::fs::read_to_string(src.join(".config")).unwrap_or_default();
    cfg.lines().any(|l| {
        l.starts_with(&format!("CONFIG_{}=", symbol))
            || l.trim() == format!("# CONFIG_{} is not set", symbol)
    })
}

/// Vrai si le symbole est présent et actif dans `.config`.
pub fn config_has(src: &Path, symbol: &str) -> bool {
    let cfg = std::fs::read_to_string(src.join(".config")).unwrap_or_default();
    cfg.lines().any(|l| {
        l.strip_prefix(&format!("CONFIG_{}=", symbol))
            .map(|v| v == "y" || v == "m")
            .unwrap_or(false)
    })
}

/// Modèle de préemption actif, lu dans `.config`.
pub fn active_preempt(src: &Path) -> Option<String> {
    for sym in ["PREEMPT_RT", "PREEMPT_LAZY", "PREEMPT_NONE", "PREEMPT_VOLUNTARY", "PREEMPT"] {
        if config_has(src, sym) {
            return Some(sym.to_string());
        }
    }
    None
}

/// Appelle `scripts/config` des sources.
fn scripts_config(src: &Path, args: &[&str]) -> Result<(), String> {
    let script = src.join("scripts/config");
    if !script.exists() {
        return Err(format!(
            "{} introuvable : les sources sont incomplètes",
            script.display()
        ));
    }
    let (code, out, err) = run_in(src, "scripts/config", args)?;
    if code != 0 {
        return Err(format!(
            "scripts/config {} a échoué : {}",
            args.join(" "),
            tail(&err)
        ));
    }
    let _ = out;
    Ok(())
}

/// `make -s kernelrelease` : la valeur exacte de `uname -r` après installation.
///
/// `make -s syncconfig` d'abord, et ce n'est pas décoratif : `scripts/config`
/// et `olddefconfig` modifient `.config` sans toujours rafraîchir
/// `include/config/auto.conf`, et `kernelrelease` lit alors l'ANCIENNE valeur.
/// Vérifié sur la VM : le `.config` contenait bien
/// `CONFIG_LOCALVERSION="-kernelorg"` et `make -s kernelrelease` répondait
/// quand même `6.12.118` — le noyau aurait été installé sans suffixe, donc en
/// collision avec celui de la distribution.
pub fn make_kernelrelease(src: &Path) -> Result<String, String> {
    let (sc, _so, se) = run_in(src, "make", &["-s", "syncconfig"])?;
    if sc != 0 {
        return Err(format!(
            "make syncconfig a échoué (code {}) : {}",
            sc,
            tail(&se)
        ));
    }
    let (code, out, err) = run_in(src, "make", &["-s", "kernelrelease"])?;
    if code != 0 {
        return Err(format!("make kernelrelease a échoué : {}", tail(&err)));
    }
    let v = out.trim().lines().last().unwrap_or("").trim().to_string();
    if v.is_empty() || !v.chars().all(|c| c.is_ascii_alphanumeric() || "+-._".contains(c)) {
        return Err(format!("version de noyau inattendue : « {} »", v));
    }
    Ok(v)
}

/// `make <cible>` silencieux (sortie journalisée seulement en cas d'échec).
fn make_quiet(src: &Path, targets: &[&str], ui: &dyn BuildUi) -> Result<(), String> {
    let mut args: Vec<&str> = vec!["-s"];
    args.extend_from_slice(targets);
    ui.log(&format!("make {}", targets.join(" ")));
    let (code, out, err) = run_in(src, "make", &args)?;
    // En cas de succès la sortie n'a pas d'intérêt ; en cas d'échec, c'est la
    // seule chose qui permette de comprendre (en-tête manquant, etc.).
    let limit = if code == 0 { 3 } else { 25 };
    for l in out.lines().filter(|l| !l.trim().is_empty()).take(limit) {
        ui.log(&format!("  {}", l));
    }
    if code != 0 {
        for l in err.lines().filter(|l| !l.trim().is_empty()).take(limit) {
            ui.log(&format!("  {}", l));
        }
        return Err(format!(
            "make {} a échoué (code {}) : {}",
            targets.join(" "),
            code,
            tail(&err)
        ));
    }
    Ok(())
}

// ----------------------------------------------------------------------
// Compilation
// ----------------------------------------------------------------------

/// Ligne de `make` digne d'être journalisée en direct.
///
/// Une compilation complète produit des dizaines de milliers de lignes ; les
/// envoyer toutes au `QPlainTextEdit` fige l'interface. On garde les actions
/// structurantes (CC/LD/AR/…) et tout ce qui ressemble à une erreur.
pub fn is_interesting_make_line(line: &str) -> bool {
    let t = line.trim_start();
    let lower = t.to_ascii_lowercase();
    if lower.contains("error") || lower.contains("warning") || lower.contains("fatal") {
        return true;
    }
    let first = t.split_whitespace().next().unwrap_or("");
    matches!(
        first,
        "CC" | "LD"
            | "AR"
            | "AS"
            | "GEN"
            | "CHK"
            | "UPD"
            | "CALL"
            | "LDS"
            | "OBJCOPY"
            | "WRAP"
            | "SORTTAB"
            | "SYSMAP"
            | "BTF"
            | "BTFIDS"
            | "MKELF"
            | "VDSOSYM"
            | "VDSOA"
            | "VDSO"
            | "HOSTCC"
            | "HOSTLD"
            | "INSTALL"
            | "DESCEND"
            | "SYNC"
            | "RELOCS"
            | "SIT"
            | "ZOFFSETS"
            | "OFFSETS"
            | "Kernel:"
            | "Image"
            | "Building"
    )
}

/// Vrai si la ligne correspond à la compilation d'un objet (compteur de
/// progression honnête : « N objets compilés »).
pub fn is_object_compile(line: &str) -> bool {
    let t = line.trim_start();
    let first = t.split_whitespace().next().unwrap_or("");
    matches!(first, "CC" | "AS" | "HOSTCC")
}

/// Lance `make -jN bzImage modules` en flux, annulable.
pub fn build(
    src: &Path,
    opts: &BuildOptions,
    ui: &dyn BuildUi,
    cancel: &Arc<AtomicBool>,
) -> Result<(), String> {
    let jobs = opts.jobs.max(1).to_string();
    let args: Vec<&str> = vec!["-j", &jobs, "bzImage", "modules"];
    let mut envs: Vec<(String, String)> = Vec::new();
    if opts.march_native {
        envs.push(("KCFLAGS".to_string(), "-march=native".to_string()));
    }
    ui.log(&format!(
        "Compilation : make -j{} bzImage modules{}",
        jobs,
        if opts.march_native {
            " (KCFLAGS=-march=native)"
        } else {
            ""
        }
    ));
    run_streaming(src, "make", &args, &envs, None, ui, cancel, true)?;
    Ok(())
}

/// `sudo -S make modules_install` : installe `/lib/modules/<kver>`.
/// Nécessite le mot de passe root (écrit sur stdin, jamais en argument).
pub fn modules_install(
    src: &Path,
    password: &str,
    ui: &dyn BuildUi,
    cancel: &Arc<AtomicBool>,
) -> Result<(), String> {
    ui.log("make modules_install (sudo) …");
    run_streaming(
        src,
        "sudo",
        &["-S", "make", "modules_install"],
        &[],
        Some(password),
        ui,
        cancel,
        false,
    )
    .map(|_| ())
}

/// chemin du `bzImage` produit.
pub fn bzimage_path(src: &Path) -> PathBuf {
    src.join("arch/x86/boot/bzImage")
}

// ----------------------------------------------------------------------
// Orchestration
// ----------------------------------------------------------------------

/// Un travail préparé : sources extraites et `.config` généré.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Prepared {
    /// Version kernel.org (ex. `6.12.118`).
    pub version: String,
    /// Répertoire des sources extraites.
    pub src: PathBuf,
    /// `uname -r` prévu (issu de `make -s kernelrelease`).
    pub kver: String,
    /// Compte rendu de configuration.
    pub report: ConfigReport,
    /// L'utilisateur a ouvert `menuconfig` : sa configuration fait foi et les
    /// options de l'onglet ne sont plus réappliquées.
    pub manual_edit: bool,
    /// Options réellement utilisées pour cette préparation. La compilation
    /// réutilise CELLES-CI et non les widgets : changer le suffixe ou
    /// `-march` après coup produirait un noyau dont le nom ne correspond plus
    /// à ce qui a été configuré.
    pub opts: BuildOptions,
}

/// Télécharge, vérifie, extrait et configure. Étape « préparer » de l'onglet
/// comme du mode `--kernelorg-stage`.
pub fn prepare(
    rel: &crate::source::OrgRelease,
    opts: &BuildOptions,
    ui: &dyn BuildUi,
    cancel: &Arc<AtomicBool>,
    allow_unsigned: bool,
) -> Result<Prepared, String> {
    // 1. Pré-vol : inutile de télécharger 150 Mo si `gcc` manque.
    ui.status(2, "Vérification des prérequis…");
    let pf = preflight(&opts.build_root, opts.march_native);
    for n in &pf.notes {
        ui.log(n);
    }
    if let Some(p) = pf.problem() {
        return Err(format!("Prérequis manquants — {}", p));
    }
    if let Some(free) = pf.free {
        ui.log(&format!(
            "Espace disponible dans le répertoire de travail : {}",
            crate::fmt_bytes(free)
        ));
    }
    cancelled(cancel)?;

    crate::sysutil::log(&format!(
        "Préparation {} : base = {}",
        rel.version,
        opts.base.label()
    ));

    // 2. Téléchargement + signature PGP.
    let work = work_dir(&opts.build_root, &rel.version);
    ui.status(5, "Téléchargement de l'archive kernel.org…");
    let (tarball, sig) = crate::source::download(rel, &work, &|recv, total| {
        let pct = if total > 0 {
            5 + ((recv * 20 / total) as i32).min(20)
        } else {
            10
        };
        ui.status(
            pct,
            &format!(
                "Téléchargement {} / {}",
                crate::fmt_bytes(recv),
                if total > 0 { crate::fmt_bytes(total) } else { "?".into() }
            ),
        );
    }, !allow_unsigned)?;
    cancelled(cancel)?;

    ui.status(27, "Vérification de la signature PGP…");
    match &sig {
        Some(sig) => {
            let key = crate::source::verify_signature(&tarball, sig)?;
            let first_time = crate::source::remember_key(&key, &rel.version).unwrap_or(false);
            ui.log(&crate::source::describe_key(&key, first_time));
        }
        None => {
            if !allow_unsigned {
                return Err(format!(
                    "aucune signature PGP disponible pour {} : compilation refusée. \
                     (Les versions -rc ne sont pas signées par kernel.org ; utilisez une \
                     version stable, ou acceptez explicitement le risque.)",
                    rel.version
                ));
            }
            ui.log(
                "ATTENTION : compilation d'une archive NON VÉRIFIÉE (aucune signature PGP \
                 publiée pour cette version). Vous avez accepté ce risque explicitement.",
            );
        }
    }
    ui.log(&format!(
        "SHA256 de l'archive : {}",
        crate::source::sha256_file(&tarball).unwrap_or_else(|_| "illisible".into())
    ));
    cancelled(cancel)?;

    // 3. Extraction.
    ui.status(32, "Extraction de l'archive…");
    let src = extract(&tarball, &work, ui)?;
    cancelled(cancel)?;

    // 4. Configuration.
    ui.status(38, "Préparation de la configuration…");
    let report = prepare_config(&src, opts, ui)?;
    for i in &report.ignored {
        ui.log(&format!("Option ignorée : {}", i));
    }
    ui.log(&format!("Version du noyau à installer : {}", report.kver));

    // Cible déjà installée ? On se fie au REGISTRE (écrit à la toute fin de
    // `finish_install`) et non à `/lib/modules/<kver>` : après un échec partiel
    // (modules installés, entrée de boot manquante), le répertoire existe déjà
    // et le refus empêchait toute reprise.
    if crate::sysutil::registry_kind(&report.kver).is_some() {
        return Err(format!(
            "le noyau {} est déjà installé. Choisissez un autre suffixe de version \
             ou supprimez-le d'abord.",
            report.kver
        ));
    }

    Ok(Prepared {
        version: rel.version.clone(),
        src,
        kver: report.kver.clone(),
        report,
        manual_edit: false,
        opts: opts.clone(),
    })
}

/// Compile puis installe un travail déjà préparé. Retourne
/// `(uname -r, entrée de boot)`.
pub fn build_and_install(
    password: &str,
    prepared: &Prepared,
    ui: &dyn BuildUi,
    cancel: &Arc<AtomicBool>,
) -> Result<(String, String), String> {
    let opts = &prepared.opts;
    cancelled(cancel)?;

    // Si l'utilisateur est passé par `menuconfig`, sa configuration fait foi :
    // on relit seulement la version cible au lieu de réécrire le `.config`.
    let kver = if prepared.manual_edit {
        ui.log("Configuration éditée à la main (menuconfig) : options de l'onglet non réappliquées");
        make_kernelrelease(&prepared.src)?
    } else {
        prepared.kver.clone()
    };
    if crate::sysutil::registry_kind(&kver).is_some() {
        return Err(format!(
            "le noyau {} est déjà installé. Rien n'a été fait.",
            kver
        ));
    }

    ui.status(45, "Compilation (long : de 30 min à plusieurs heures)…");
    let t0 = Instant::now();
    build(&prepared.src, opts, ui, cancel)?;
    let bz = bzimage_path(&prepared.src);
    if !bz.exists() {
        return Err(format!(
            "compilation terminée mais {} est absent — rien n'a été installé",
            bz.display()
        ));
    }
    ui.log(&format!(
        "Compilation terminée en {} — bzImage : {}",
        human_duration(t0.elapsed()),
        crate::fmt_bytes(bz.metadata().map(|m| m.len()).unwrap_or(0))
    ));
    cancelled(cancel)?;

    ui.status(90, "Installation des modules (sudo)…");
    modules_install(&prepared.src, password, ui, cancel)?;

    ui.status(92, "Création de l'initrd et de l'entrée de boot…");
    let entry = crate::install::finish_install(
        password,
        &kver,
        &bz,
        crate::repo::KernelKind::KernelOrg,
        &format!("{}{}", prepared.version, opts.localversion.trim()),
        &|i, text| {
            // Étapes 3..8 de finish_install → 92..100 %.
            let pct = 92 + (i.min(8) as i32);
            ui.status(pct, text);
        },
    )?;
    ui.status(100, "Terminé ✓");
    Ok((kver, entry))
}

/// Travail complet en un appel (mode CLI) : préparation puis compilation.
pub fn run_job(
    password: &str,
    rel: &crate::source::OrgRelease,
    opts: &BuildOptions,
    ui: &dyn BuildUi,
    cancel: &Arc<AtomicBool>,
) -> Result<(String, String), String> {
    let p = prepare(rel, opts, ui, cancel, false)?;
    build_and_install(password, &p, ui, cancel)
}

/// Erreur d'annulation.
fn cancelled(cancel: &Arc<AtomicBool>) -> Result<(), String> {
    if cancel.load(Ordering::Relaxed) {
        Err("Opération annulée par l'utilisateur.".to_string())
    } else {
        Ok(())
    }
}

/// Durée lisible : `2 h 13 min`, `14 min 05 s`, `38 s`.
pub fn human_duration(d: Duration) -> String {
    let s = d.as_secs();
    if s >= 3600 {
        format!("{} h {:02} min", s / 3600, (s % 3600) / 60)
    } else if s >= 60 {
        format!("{} min {:02} s", s / 60, s % 60)
    } else {
        format!("{} s", s)
    }
}

// ----------------------------------------------------------------------
// menuconfig / nettoyage
// ----------------------------------------------------------------------

/// Terminaux graphiques essayés pour `make menuconfig`, dans l'ordre.
///
/// `menuconfig` a besoin d'un vrai terminal : un `Command` lancé depuis le fil
/// Qt n'en fournit pas. On ouvre donc un émulateur de terminal sur la cible.
const TERMINALS: &[(&str, &[&str])] = &[
    ("konsole", &["-e", "make", "menuconfig"]),
    ("xterm", &["-e", "make", "menuconfig"]),
    ("alacritty", &["-e", "make", "menuconfig"]),
    ("kitty", &["make", "menuconfig"]),
    ("xfce4-terminal", &["-x", "make", "menuconfig"]),
    ("gnome-terminal", &["--", "make", "menuconfig"]),
    ("mate-terminal", &["-x", "make", "menuconfig"]),
    ("tilix", &["-e", "make", "menuconfig"]),
];

/// Commande d'ouverture de `menuconfig` : `(programme, arguments)`.
/// Le répertoire courant doit être celui des sources.
pub fn menuconfig_command() -> Result<(String, Vec<String>), String> {
    for (bin, args) in TERMINALS {
        if crate::source::has_bin(bin) {
            return Ok((
                (*bin).to_string(),
                args.iter().map(|a| (*a).to_string()).collect(),
            ));
        }
    }
    Err(
        "aucun terminal graphique trouvé (konsole, xterm, alacritty, kitty, xfce4-terminal, \
         gnome-terminal). Ouvrez `make menuconfig` à la main dans le répertoire des sources."
            .to_string(),
    )
}

/// Taille récursive d'un répertoire (octets, sans suivre les liens).
pub fn dir_size(dir: &Path) -> u64 {
    let mut total = 0u64;
    let entries = match std::fs::read_dir(dir) {
        Ok(e) => e,
        Err(_) => return 0,
    };
    for e in entries.flatten() {
        match e.metadata() {
            Ok(m) if m.is_dir() => total += dir_size(&e.path()),
            Ok(m) => total += m.len(),
            Err(_) => {}
        }
    }
    total
}

/// Supprime le répertoire de travail d'une version. Retourne les octets
/// libérés (estimation) et une erreur si la suppression échoue.
pub fn clean_sources(build_root: &Path, version: &str) -> Result<u64, String> {
    let dir = work_dir(build_root, version);
    if !dir.exists() {
        return Ok(0);
    }
    let freed = dir_size(&dir);
    std::fs::remove_dir_all(&dir)
        .map_err(|e| format!("suppression de {} impossible : {}", dir.display(), e))?;
    Ok(freed)
}

// ----------------------------------------------------------------------
// Exécution de commandes
// ----------------------------------------------------------------------

/// Lance une commande dans `dir` et capture la sortie.
fn run_in(dir: &Path, cmd: &str, args: &[&str]) -> Result<(i32, String, String), String> {
    let out = Command::new(cmd)
        .args(args)
        .current_dir(dir)
        .env("LC_ALL", "C")
        .output()
        .map_err(|e| format!("impossible de lancer {} : {}", cmd, e))?;
    Ok((
        out.status.code().unwrap_or(-1),
        String::from_utf8_lossy(&out.stdout).to_string(),
        String::from_utf8_lossy(&out.stderr).to_string(),
    ))
}

fn run(cmd: &str, args: &[&str]) -> (i32, String, String) {
    match Command::new(cmd).args(args).output() {
        Ok(o) => (
            o.status.code().unwrap_or(-1),
            String::from_utf8_lossy(&o.stdout).to_string(),
            String::from_utf8_lossy(&o.stderr).to_string(),
        ),
        Err(e) => (-1, String::new(), e.to_string()),
    }
}

/// Lance une commande en flux : chaque ligne stdout/stderr est poussée au
/// journal (filtrée ou non), la progression par objets est publiée dans le
/// statut, et un `cancel` tue le processus.
fn run_streaming(
    dir: &Path,
    cmd: &str,
    args: &[&str],
    envs: &[(String, String)],
    stdin_data: Option<&str>,
    ui: &dyn BuildUi,
    cancel: &Arc<AtomicBool>,
    filter: bool,
) -> Result<(), String> {
    let mut c = Command::new(cmd);
    c.args(args)
        .current_dir(dir)
        .env("LC_ALL", "C")
        .env_remove("MAKEFLAGS")
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .stdin(Stdio::piped());
    for (k, v) in envs {
        c.env(k, v);
    }
    let mut child = c
        .spawn()
        .map_err(|e| format!("impossible de lancer {} : {}", cmd, e))?;

    // stdin : le mot de passe sudo quand il y en a un (jamais en argument de
    // commande, donc invisible dans `ps`), sinon fermé — c'est ce qui fait
    // que `localmodconfig` prend les valeurs par défaut au lieu de bloquer.
    match stdin_data {
        Some(pw) => {
            if let Some(mut stdin) = child.stdin.take() {
                let _ = stdin.write_all(pw.as_bytes());
                let _ = stdin.write_all(b"\n");
            }
        }
        None => drop(child.stdin.take()),
    }

    let (tx, rx) = mpsc::channel::<String>();
    for pipe in [
        child.stdout.take().map(|p| Box::new(p) as Box<dyn std::io::Read + Send>),
        child.stderr.take().map(|p| Box::new(p) as Box<dyn std::io::Read + Send>),
    ]
    .into_iter()
    .flatten()
    {
        let tx = tx.clone();
        std::thread::spawn(move || {
            let r = BufReader::new(pipe);
            for line in r.lines() {
                match line {
                    Ok(l) => {
                        if tx.send(l).is_err() {
                            break;
                        }
                    }
                    Err(_) => break,
                }
            }
        });
    }
    drop(tx);

    let t0 = Instant::now();
    let mut objects: u64 = 0;
    let mut last_status = Instant::now();
    let mut aborted = false;
    // Anneau des dernières lignes BRUTES : le filtre masque les erreurs
    // (une ligne `error:` n'a pas de préfixe CC/LD), et une compilation qui
    // échoue affichait « Voir le journal » sur un journal vide. On garde donc
    // de quoi reconstituter la fin de la sortie réelle.
    let mut raw_tail: std::collections::VecDeque<String> = std::collections::VecDeque::new();
    const RAW_TAIL_LINES: usize = 40;
    loop {
        match rx.recv_timeout(Duration::from_millis(200)) {
            Ok(line) => {
                if is_object_compile(&line) {
                    objects += 1;
                }
                if !line.trim().is_empty() {
                    raw_tail.push_back(line.clone());
                    if raw_tail.len() > RAW_TAIL_LINES {
                        raw_tail.pop_front();
                    }
                }
                if !filter || is_interesting_make_line(&line) {
                    ui.log(&line);
                }
                if last_status.elapsed() >= Duration::from_secs(5) {
                    ui.status(
                        46,
                        &format!(
                            "Compilation : {} objets — {} écoulées",
                            objects,
                            human_duration(t0.elapsed())
                        ),
                    );
                    last_status = Instant::now();
                }
            }
            Err(mpsc::RecvTimeoutError::Timeout) => {
                if cancel.load(Ordering::Relaxed) {
                    let _ = child.kill();
                    aborted = true;
                    break;
                }
                if let Ok(Some(_)) = child.try_wait() {
                    // Processus terminé et canaux en cours de vidage : on
                    // continue à lire jusqu'à Disconnected.
                    continue;
                }
            }
            Err(mpsc::RecvTimeoutError::Disconnected) => break,
        }
    }
    let status = child
        .wait()
        .map_err(|e| format!("attente de {} impossible : {}", cmd, e))?;
    if aborted || cancel.load(Ordering::Relaxed) {
        return Err("Opération annulée par l'utilisateur.".to_string());
    }
    if !status.success() {
        // On recopie la vraie fin de sortie dans le journal, puis la ligne la
        // plus parlante dans le message d'erreur.
        ui.log(&format!(
            "---- sortie de {} (dernières lignes) ----",
            cmd
        ));
        for l in &raw_tail {
            ui.log(l);
        }
        // Les lignes `make: *** [Makefile:248: __sub-make] Error 2` ne disent
        // RIEN : c'est la conclusion de make, pas la cause (elle a été imprimée
        // par le sous-make juste avant). On les écarte et on garde les
        // dernières lignes qui ressemblent à un vrai diagnostic.
        let is_make_trailer = |l: &str| {
            let t = l.trim_start();
            t.starts_with("make: ***")
                || t.starts_with("make[1]: ***")
                || t.starts_with("make[2]: ***")
                || t.starts_with("make[3]: ***")
        };
        let meaningful: Vec<&String> = raw_tail
            .iter()
            .filter(|l| !is_make_trailer(l) && !l.trim().is_empty())
            .collect();
        let hint = meaningful
            .iter()
            .rev()
            .find(|l| {
                let low = l.to_ascii_lowercase();
                low.contains("error")
                    || low.contains("erreur")
                    || low.contains("fatal")
                    || low.contains("no such file")
                    || low.contains("not found")
                    || low.contains("introuvable")
                    || low.contains("cannot")
                    || low.contains("impossible")
            })
            .copied()
            .or_else(|| meaningful.last().copied())
            .or_else(|| raw_tail.back())
            .map(|l| {
                let t = l.trim();
                if t.chars().count() > 220 {
                    let mut s: String = t.chars().take(220).collect();
                    s.push('…');
                    s
                } else {
                    t.to_string()
                }
            })
            .unwrap_or_default();
        return Err(format!(
            "{} {} a échoué (code {}).\n\n{}\n\n(les dernières lignes sont aussi dans le journal)",
            cmd,
            args.join(" "),
            status.code().unwrap_or(-1),
            hint
        ));
    }
    ui.log(&format!(
        "{} {} terminé en {} ({} objets)",
        cmd,
        args.join(" "),
        human_duration(t0.elapsed()),
        objects
    ));
    Ok(())
}

fn tail(s: &str) -> String {
    let all: Vec<&str> = s.lines().filter(|l| !l.trim().is_empty()).collect();
    let start = all.len().saturating_sub(4);
    all[start..].join(" | ")
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::cell::RefCell;

    /// UI de test : capture journal + statuts.
    struct RecUi {
        log: RefCell<Vec<String>>,
        status: RefCell<Vec<(i32, String)>>,
    }
    impl RecUi {
        fn new() -> Self {
            Self {
                log: RefCell::new(Vec::new()),
                status: RefCell::new(Vec::new()),
            }
        }
    }
    impl BuildUi for RecUi {
        fn log(&self, line: &str) {
            self.log.borrow_mut().push(line.to_string());
        }
        fn status(&self, pct: i32, text: &str) {
            self.status.borrow_mut().push((pct, text.to_string()));
        }
    }

    #[test]
    fn source_dir_name_strips_archive_suffix() {
        assert_eq!(
            source_dir_name("linux-6.12.118.tar.xz").as_deref(),
            Some("linux-6.12.118")
        );
        // Une archive gzip (RC sur git.kernel.org) doit aussi être reconnue.
        assert_eq!(
            source_dir_name("linux-6.12.118.tar.gz"),
            Some("linux-6.12.118".to_string())
        );
        assert_eq!(source_dir_name("linux-6.12.118.tar.bz2"), Some("linux-6.12.118".into()));
        assert_eq!(source_dir_name("linux-6.12.118.zip"), None);
        assert_eq!(source_dir_name(""), None);
    }

    #[test]
    fn work_dir_uses_version() {
        let w = work_dir(Path::new("/tmp/b"), "6.12.118");
        assert_eq!(w, PathBuf::from("/tmp/b/linux-6.12.118"));
    }

    #[test]
    fn default_options_are_safe() {
        let o = BuildOptions::default();
        assert_eq!(o.localversion, "-kernelorg");
        assert_eq!(o.preempt, Preempt::Keep);
        assert!(o.jobs >= 1);
        assert!(!o.build_root.as_os_str().is_empty());
    }

    #[test]
    fn preempt_symbols() {
        assert_eq!(Preempt::Keep.symbol(), None);
        assert_eq!(Preempt::Lazy.symbol(), Some("PREEMPT_LAZY"));
        assert_eq!(Preempt::from_str("full"), Some(Preempt::Full));
        assert_eq!(Preempt::from_str("nope"), None);
    }

    #[test]
    fn config_base_from_str() {
        assert_eq!(ConfigBase::parse("localmod"), Some(ConfigBase::LocalMod));
        assert_eq!(ConfigBase::parse("CURRENT"), Some(ConfigBase::Current));
        assert_eq!(ConfigBase::parse("x"), None);
    }

    #[test]
    fn interesting_lines_filter_keeps_actions_and_errors() {
        assert!(is_interesting_make_line("  CC      kernel/sched/core.o"));
        assert!(is_interesting_make_line("  LD      vmlinux"));
        assert!(is_interesting_make_line(
            "kernel/foo.c:12:5: error: something"
        ));
        assert!(is_interesting_make_line("warning: unused variable"));
        assert!(!is_interesting_make_line("  SPLIT   include/linux/x.h"));
        assert!(!is_interesting_make_line(""));
    }

    #[test]
    fn object_counter_only_counts_compiles() {
        assert!(is_object_compile("  CC [M]  drivers/net/foo.o"));
        assert!(is_object_compile("  AS      arch/x86/entry/entry.o"));
        assert!(!is_object_compile("  LD      vmlinux"));
    }

    #[test]
    fn human_duration_formats() {
        assert_eq!(human_duration(Duration::from_secs(38)), "38 s");
        assert_eq!(human_duration(Duration::from_secs(845)), "14 min 05 s");
        assert_eq!(human_duration(Duration::from_secs(7980)), "2 h 13 min");
    }

    #[test]
    fn preflight_reports_missing_tools() {
        // `bc` est quasi toujours absent de cet environnement de dev : on ne
        // teste que la cohérence du verdict, pas la liste exacte.
        let pf = preflight(Path::new("."), false);
        assert!(pf.free.is_some());
        // `problem()` doit être cohérent avec les deux causes possibles.
        assert_eq!(
            pf.problem().is_none(),
            pf.missing.is_empty() && pf.free.unwrap() >= MIN_FREE_BYTES
        );
        if !pf.missing.is_empty() {
            assert!(pf.problem().unwrap().contains("outils manquants"));
        }
    }

    #[test]
    fn extract_rejects_unknown_archive_name() {
        let ui = RecUi::new();
        let e = extract(Path::new("/nonexistent/foo.tar.gz"), Path::new("/tmp/x"), &ui);
        assert!(e.is_err());
    }

    #[test]
    fn prepare_config_refuses_empty_localversion() {
        let ui = RecUi::new();
        let mut o = BuildOptions::default();
        o.localversion = "  ".to_string();
        // La base `Defconfig` échouerait avant (pas de sources) : on vérifie le
        // refus du suffixe séparément, sans toucher au disque.
        assert!(validate_localversion(&o.localversion).is_err());
        let _ = prepare_config(Path::new("/nonexistent"), &o, &ui);
    }

    /// Écrit un `.config` minimal dans un répertoire temporaire.
    fn tmp_src(tag: &str, config: &str) -> PathBuf {
        let d = std::env::temp_dir().join(format!("sekm-{}-{}", tag, std::process::id()));
        let _ = std::fs::remove_dir_all(&d);
        std::fs::create_dir_all(&d).unwrap();
        if !config.is_empty() {
            std::fs::write(d.join(".config"), config).unwrap();
        }
        d
    }

    #[test]
    fn config_has_reads_active_and_module_symbols() {
        let d = tmp_src(
            "cfg",
            "CONFIG_PREEMPT_LAZY=y\nCONFIG_MODULES=m\nCONFIG_UNSET=n\n# CONFIG_NONE is not set\n",
        );
        assert!(config_has(&d, "PREEMPT_LAZY"));
        assert!(config_has(&d, "MODULES"));
        assert!(!config_has(&d, "UNSET"));
        assert!(!config_has(&d, "NONE"));
        assert!(!config_has(&d, "ABSENT"));
        let _ = std::fs::remove_dir_all(&d);
    }

    #[test]
    fn active_preempt_reports_the_model_in_the_file() {
        let d = tmp_src("preempt", "CONFIG_PREEMPT_NONE=y\n");
        assert_eq!(active_preempt(&d).as_deref(), Some("PREEMPT_NONE"));
        let d2 = tmp_src("preempt2", "CONFIG_PREEMPT=y\n");
        assert_eq!(active_preempt(&d2).as_deref(), Some("PREEMPT"));
        let d3 = tmp_src("preempt3", "CONFIG_X=y\n");
        assert_eq!(active_preempt(&d3), None);
        // `.config` absent : pas de modèle, pas de panique.
        let d4 = tmp_src("preempt4", "");
        assert_eq!(active_preempt(&d4), None);
        assert!(!config_has(&d4, "PREEMPT"));
        for d in [d, d2, d3, d4] {
            let _ = std::fs::remove_dir_all(&d);
        }
    }

    #[test]
    fn completeness_pass_enables_essentials_after_defconfig() {
        let src = fake_kernel_tree("essentials", true);
        let ui = RecUi::new();
        let mut o = BuildOptions::default();
        o.base = ConfigBase::Defconfig;
        let r = prepare_config(&src, &o, &ui).unwrap();
        let cfg = std::fs::read_to_string(src.join(".config")).unwrap();
        assert!(cfg.contains("CONFIG_ZRAM=m"), "zram attendu en module :\n{}", cfg);
        assert!(
            cfg.contains("CONFIG_SND_HDA_CODEC_REALTEK=m"),
            "codec audio attendu en module :\n{}",
            cfg
        );
        assert!(r.modules >= 2, "modules = {}", r.modules);
        let _ = std::fs::remove_dir_all(&src);
    }

    #[test]
    fn fs_symbol_mapping() {
        assert_eq!(fs_symbol_for("btrfs"), Some("BTRFS_FS"));
        assert_eq!(fs_symbol_for("EXT4"), Some("EXT4_FS"));
        assert_eq!(fs_symbol_for("xfs"), Some("XFS_FS"));
        assert_eq!(fs_symbol_for("zfs"), None);
    }

    #[test]
    fn free_bytes_nearest_walks_up_from_a_missing_path() {
        // Chemin profondément inexistant : on doit quand même obtenir la
        // capacité du système de fichiers ancêtre (bug #1 des tickets).
        let p = Path::new("/tmp/sekm-inexistant-a/b/c/d");
        let f = free_bytes_nearest(p);
        assert!(f.is_some(), "un chemin absent ne doit pas rendre le disque illisible");
        assert!(f.unwrap() > 0);
        // Chemin existant.
        assert!(free_bytes_nearest(Path::new("/tmp")).is_some());
    }

    #[test]
    fn dir_size_and_clean_sources() {
        let root = std::env::temp_dir().join(format!("sekm-clean-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&root);
        let w = work_dir(&root, "6.12.118");
        std::fs::create_dir_all(w.join("sub")).unwrap();
        std::fs::write(w.join("a"), b"12345").unwrap();
        std::fs::write(w.join("sub/b"), b"678").unwrap();
        assert_eq!(dir_size(&w), 8);
        assert_eq!(clean_sources(&root, "6.12.118"), Ok(8));
        assert!(!w.exists());
        // Nettoyer deux fois ne casse rien.
        assert_eq!(clean_sources(&root, "6.12.118"), Ok(0));
        let _ = std::fs::remove_dir_all(&root);
    }

    /// Faux arbre de sources : un `Makefile` qui répond aux cibles utilisées
    /// par `prepare_config`, et un `scripts/config` qui imite le vrai.
    ///
    /// `olddefconfig` ne garde que les symboles « connus » du faux Kconfig, et
    /// le `.config` de départ liste les booléens sous les DEUX formes
    /// (`CONFIG_X=y` / `# CONFIG_X is not set`), comme un vrai defconfig.
    /// `with_lazy=false` reproduit un noyau antérieur à `PREEMPT_LAZY` (6.12).
    ///
    /// Cela vérifie l'ORDRE des opérations et la relecture du `.config` sans
    /// compiler un noyau (flex/bison ne sont pas garantis sur la machine).
    fn fake_kernel_tree(tag: &str, with_lazy: bool) -> PathBuf {
        use std::os::unix::fs::PermissionsExt;
        let d = std::env::temp_dir().join(format!("sekm-fake-{}-{}", tag, std::process::id()));
        let _ = std::fs::remove_dir_all(&d);
        std::fs::create_dir_all(d.join("scripts")).unwrap();

        let mut defcfg = String::from(
            "CONFIG_PREEMPT_NONE=y\\nCONFIG_PREEMPT_DYNAMIC=y\\n# CONFIG_PREEMPT_VOLUNTARY is not set\\n# CONFIG_PREEMPT is not set\\n",
        );
        if with_lazy {
            defcfg.push_str("# CONFIG_PREEMPT_LAZY is not set\\n");
        }
        // Deux sous-systèmes essentiels explicitement désactivés, comme dans le
        // vrai defconfig : la passe de complétude doit les rallumer.
        defcfg.push_str("# CONFIG_ZRAM is not set\\n# CONFIG_SND_HDA_CODEC_REALTEK is not set\\n");
        // printf avec un échappement contrôlé du contenu
        let makefile = format!(
            ".PHONY: defconfig olddefconfig syncconfig kernelrelease localmodconfig\n\
             defconfig:\n\
             \t@printf '%b' '{}' > .config\n\
             localmodconfig:\n\
             \t@true\n\
             olddefconfig:\n\
             \t@sh ./scripts/trim .config\n\
             syncconfig:\n\
             \t@true\n\
             kernelrelease:\n\
             \t@printf '6.12.118%s\\n' \"$$(sed -n 's/^CONFIG_LOCALVERSION=\"\\(.*\\)\"$$/\\1/p' .config)\"\n",
            defcfg
        );
        std::fs::write(d.join("Makefile"), makefile).unwrap();

        let cfg = "\
#!/bin/sh\n\
cfg=.config\n\
drop() { sed -i -e \"/^CONFIG_$1=/d\" -e \"/^# CONFIG_$1 is not set/d\" \"$cfg\"; }\n\
case \"$1\" in\n\
--set-str)\n\
  drop \"$2\"\n\
  printf 'CONFIG_%s=\"%s\"\\n' \"$2\" \"$3\" >> \"$cfg\" ;;\n\
--enable)\n\
  drop \"$2\"\n\
  printf 'CONFIG_%s=y\\n' \"$2\" >> \"$cfg\" ;;\n\
--disable)\n\
  drop \"$2\" ;;\n\
--module)\n\
  drop \"$2\"\n\
  printf 'CONFIG_%s=m\\n' \"$2\" >> \"$cfg\" ;;\n\
esac\n";
        let cfgp = d.join("scripts/config");
        std::fs::write(&cfgp, cfg).unwrap();
        std::fs::set_permissions(&cfgp, std::fs::Permissions::from_mode(0o755)).unwrap();

        // Le « Kconfig » du faux arbre connaît les symboles de préemption, de
        // version, de FS et TOUS les essentiels (sinon la passe de complétude
        // les ignorerait faute de ligne dans .config).
        let mut keep: Vec<&str> = vec![
            "PREEMPT_NONE",
            "PREEMPT_LAZY",
            "PREEMPT",
            "PREEMPT_DYNAMIC",
            "PREEMPT_VOLUNTARY",
            "LOCALVERSION",
            "BTRFS_FS",
            "EXT4_FS",
            "XFS_FS",
            "F2FS_FS",
            "VFAT_FS",
            "NTFS3_FS",
        ];
        keep.extend(ESSENTIAL_MODULES.iter().copied());
        keep.extend(ESSENTIAL_BUILTIN.iter().copied());
        let trim = format!(
            "#!/bin/sh\ngrep -E '^(CONFIG|# CONFIG)_({})[ =]' \"$1\" > \"$1.new\"\nmv \"$1.new\" \"$1\"\n",
            keep.join("|")
        );
        let trimp = d.join("scripts/trim");
        std::fs::write(&trimp, trim).unwrap();
        std::fs::set_permissions(&trimp, std::fs::Permissions::from_mode(0o755)).unwrap();
        d
    }

    #[test]
    fn prepare_config_applies_localversion_and_reads_kver() {
        let src = fake_kernel_tree("kver", true);
        let ui = RecUi::new();
        let mut o = BuildOptions::default();
        o.base = ConfigBase::Defconfig;
        o.preempt = Preempt::Keep;
        o.localversion = "-kernelorg".to_string();
        let r = prepare_config(&src, &o, &ui).unwrap();
        assert_eq!(r.kver, "6.12.118-kernelorg");
        assert_eq!(r.preempt_active, "PREEMPT_NONE");
        assert!(r.preempt_dynamic, "PREEMPT_DYNAMIC=y doit être détecté");
        assert!(r.ignored.is_empty(), "ignored={:?}", r.ignored);
        let _ = std::fs::remove_dir_all(&src);
    }

    #[test]
    fn prepare_config_enables_available_preempt_model() {
        let src = fake_kernel_tree("lazy", true);
        let ui = RecUi::new();
        let mut o = BuildOptions::default();
        o.base = ConfigBase::Defconfig;
        o.preempt = Preempt::Lazy;
        let r = prepare_config(&src, &o, &ui).unwrap();
        assert_eq!(r.preempt_active, "PREEMPT_LAZY");
        assert!(r.ignored.is_empty(), "ignored={:?}", r.ignored);
        let cfg = std::fs::read_to_string(src.join(".config")).unwrap();
        assert!(!cfg.contains("CONFIG_PREEMPT_NONE=y"), "cfg={}", cfg);
        let _ = std::fs::remove_dir_all(&src);
    }

    #[test]
    fn prepare_config_enables_voluntary_model() {
        let src = fake_kernel_tree("vol", false);
        let ui = RecUi::new();
        let mut o = BuildOptions::default();
        o.base = ConfigBase::Defconfig;
        o.preempt = Preempt::Voluntary;
        let r = prepare_config(&src, &o, &ui).unwrap();
        assert_eq!(r.preempt_active, "PREEMPT_VOLUNTARY");
        assert!(r.ignored.is_empty(), "ignored={:?}", r.ignored);
        let _ = std::fs::remove_dir_all(&src);
    }

    #[test]
    fn prepare_config_reports_preempt_model_missing_from_version() {
        // 6.12 n'a pas PREEMPT_LAZY : on doit le signaler ET conserver le
        // modèle d'origine, sans laisser le .config sans choix.
        let src = fake_kernel_tree("old", false);
        let ui = RecUi::new();
        let mut o = BuildOptions::default();
        o.base = ConfigBase::Defconfig;
        o.preempt = Preempt::Lazy;
        let r = prepare_config(&src, &o, &ui).unwrap();
        assert_eq!(r.preempt_active, "PREEMPT_NONE", "modèle d'origine conservé");
        assert!(
            r.ignored.iter().any(|i| i.starts_with("PREEMPT_LAZY")),
            "ignored={:?}",
            r.ignored
        );
        let _ = std::fs::remove_dir_all(&src);
    }

    #[test]
    fn prepare_config_refuses_empty_localversion_on_real_tree() {
        let src = fake_kernel_tree("nolv", true);
        let ui = RecUi::new();
        let mut o = BuildOptions::default();
        o.base = ConfigBase::Defconfig;
        o.localversion = String::new();
        let e = prepare_config(&src, &o, &ui);
        assert!(e.is_err());
        assert!(e.unwrap_err().contains("LOCALVERSION"));
        let _ = std::fs::remove_dir_all(&src);
    }

    #[test]
    fn config_symbol_exists_understands_both_forms() {
        let d = tmp_src(
            "exists",
            "CONFIG_A=y\n# CONFIG_B is not set\n",
        );
        assert!(config_symbol_exists(&d, "A"));
        assert!(config_symbol_exists(&d, "B"), "forme « is not set » = connu");
        assert!(!config_symbol_exists(&d, "C"));
        let _ = std::fs::remove_dir_all(&d);
    }

    #[test]
    fn validate_localversion_accepts_expected_suffixes() {
        assert!(validate_localversion("-kernelorg").is_ok());
        assert!(validate_localversion("-src2").is_ok());
        assert!(validate_localversion("").is_err());
        assert!(validate_localversion("a b").is_err());
        assert!(validate_localversion("/x").is_err());
    }
}
