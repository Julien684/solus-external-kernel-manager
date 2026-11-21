//! Solus External Kernel Manager — GUI Qt6 (via `qtrs`) : installer/supprimer
//! les noyaux externes (CachyOS, Zen, Liquorix) sur Solus.
//!
//! Trois onglets en haut, un par noyau (CachyOS / Zen / Liquorix). Chaque
//! onglet contient sa table de versions disponibles **et ses boutons**
//! (Actualiser / Installer / Supprimer). La zone commune en bas rassemble la
//! liste des « noyaux installés », le démarrage systemd-boot, la barre de
//! progression et le journal.
//!
//! Un quatrième onglet « Avancé » compile un noyau depuis kernel.org
//! (`source.rs` pour l'archive + la signature PGP, `build.rs` pour la
//! configuration, `make` et l'installation). Il n'est pas une « famille » de
//! noyaux : pas de table de versions, mais un assistant (version → options →
//! compilation) et un journal en direct. La compilation se fait en utilisateur
//! normal ; `sudo` n'intervient que pour `modules_install` et la fin
//! d'installation, commune avec les noyaux pacman (`install::finish_install`).
//!
//! Modèle d'ownership qtrs (validé par l'exemple `editor` du crate) :
//!  - La fenêtre (MainWindow) est dans `Rc<RefCell<..>>` (clonable dans les
//!    callbacks). On y accède par auto-déréférencement (`window.borrow()`).
//!  - Les widgets « interactifs » (table, liste, barre, statut, journal) sont
//!    gardés en `Rc<RefCell<..>>`, marqués `set_has_parent()` (Qt gère leur
//!    lifetime C++ — leur `Drop` ne supprime donc pas le C++), puis
//!    enveloppés dans un `ScrollArea` via `set_widget(&dyn AsWidget)` (réf.
//!    partagée : le widget reste en Rc). Seuls les ScrollArea/layout et les
//!    widgets « statiques » (boutons, groupboxes, titres) sont « dépossédés »
//!    dans les layouts.
//!  - `build()` avec `.parent(..)` met déjà `has_parent = true` → aucun
//!    double-free, même si on garde une référence Rust.
//!  - `VBoxLayout::with_parent(..)` installe le layout sur le parent d'un coup.
//!  - `MainWindow::set_central_widget` / `set_menu_bar` prennent `&dyn`
//!    (référence partagée, pas de transfert d'ownership).
//!  - Travail long (HTTP / sudo / dracut) → `std::thread` + `mpsc` ; un
//!    `qtrs::Timer` (400 ms) fait la « pompe » file → widgets dans le thread UI.

use std::cell::RefCell;
use std::rc::Rc;
use std::sync::mpsc;

use qtrs::prelude::*;

mod build;
mod install;
mod repo;
mod source;
mod sysutil;

use build::{BuildOptions, BuildUi, ConfigBase, Preempt, Prepared};
use install::{list_installed, remove_kernel, InstalledKernel};
use repo::{download_kernel, fetch_latest_kernels, KernelKind, KernelRelease};

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;

// ------------------------------------------------------------------
// Messages worker → UI
// ------------------------------------------------------------------
enum WorkerMsg {
    Progress { pct: i32, text: String },
    Log(String),
    Done { ok: bool, message: String },
    /// Résultat d'un « Actualiser » pour un noyau donné. `tag` indexe la
    /// table/la liste de kernels du noyau correspondant.
    Fetch {
        tag: u8,
        kernels: Vec<KernelRelease>,
        installed: Vec<InstalledKernel>,
    },
    /// Liste des versions publiées sur kernel.org (onglet « Avancé »).
    OrgReleases(Vec<source::OrgRelease>),
    /// Travail préparé (sources + `.config`) — mémorisé côté UI pour les
    /// étapes suivantes (menuconfig, compilation).
    Prepared(Box<Prepared>),
}

/// `BuildUi` côté worker : chaque ligne part dans le journal, chaque statut
/// dans la barre + le libellé de statut de l'onglet.
struct ChannelUi {
    tx: mpsc::Sender<WorkerMsg>,
}

impl BuildUi for ChannelUi {
    fn log(&self, line: &str) {
        let _ = self.tx.send(WorkerMsg::Log(line.to_string()));
    }
    fn status(&self, pct: i32, text: &str) {
        let _ = self.tx.send(WorkerMsg::Progress {
            pct,
            text: text.to_string(),
        });
    }
}

// ------------------------------------------------------------------
// Helpers (thread UI)
// ------------------------------------------------------------------
fn set_status(status: &Rc<RefCell<Label>>, text: &str) {
    status.borrow_mut().set_text(text);
}

/// Met à jour le texte d'un `QLabel` à partir de son pointeur brut (utile pour
/// les labels dont on n'a pas gardé le wrapper qtrs, seulement l'adresse).
fn set_label_text(ptr: *mut qtrs::ffi::QLabel, text: &str) {
    use cxx::let_cxx_string;
    let_cxx_string!(c = text);
    unsafe { qtrs::ffi::QLabel_setText(ptr, &c) };
}

fn append_log(logw: &Rc<RefCell<PlainTextEdit>>, line: &str) {
    let ts = chrono::Local::now().format("%H:%M:%S");
    logw.borrow_mut().append_plain_text(&format!("[{}] {}\n", ts, line));
}

/// Formatage « taille humaine » (o / Ko / Mo / Go / To, base 1024).
/// Testé unitairement dans `mod tests`.
fn fmt_bytes(n: u64) -> String {
    const UNITS: [&str; 5] = ["o", "Ko", "Mo", "Go", "To"];
    let mut v = n as f64;
    let mut u = 0;
    while v >= 1024.0 && u < UNITS.len() - 1 {
        v /= 1024.0;
        u += 1;
    }
    if u == 0 {
        format!("{} {}", n, UNITS[0])
    } else {
        format!("{:.1} {}", v, UNITS[u])
    }
}

/// Référence aux widgets partagés (utilisée par les closures boutons).
struct SharedUi {
    tx: mpsc::Sender<WorkerMsg>,
    status: Rc<RefCell<Label>>,
    logw: Rc<RefCell<PlainTextEdit>>,
    /// Barre de progression commune. Pendant une opération elle passe en mode
    /// « occupé » (plage 0..0) : Qt l'anime alors en va-et-vient, ce qui montre
    /// que quelque chose se passe même quand aucun pourcentage n'existe
    /// (compilation, `eopkg`, `dracut`).
    bar: Rc<RefCell<ProgressBar>>,
}

/// Rafraîchit la liste « noyaux installés » (commune aux 3 onglets).
fn refresh_installed_list(list: &Rc<RefCell<ListWidget>>) {
    let inst = list_installed();
    let mut items: Vec<String> = Vec::new();
    for k in &inst {
        let mark = if k.running { "  ● (en cours d'exécution)" } else { "" };
        items.push(format!("{} [{}]{}", k.version, k.kind.label(), mark));
    }
    if items.is_empty() {
        items.push("(aucun noyau externe installé)".into());
    }
    let refs: Vec<&str> = items.iter().map(|s| s.as_str()).collect();
    list.borrow_mut().clear();
    list.borrow_mut().add_items(&refs);
}

/// Construit un ScrollArea qui enveloppe un widget conservé en `Rc`
/// (pattern qtrs : le ScrollArea garde une référence partagée, le widget
/// reste en Rc ; le ScrollArea est lui re-parenté par `parent` puis ajouté
/// au layout).
fn wrap_scroll(w: &Rc<RefCell<impl qtrs::AsWidget>>, parent: &dyn AsWidget) -> ScrollArea {
    let sa = ScrollArea::new().parent(parent).set_widget_resizable(true).build();
    sa.set_widget(&*w.borrow());
    sa
}

/// Crée la table (tableau 0×5) d'un noyau.
fn new_table_for(kind: KernelKind) -> Rc<RefCell<TableWidget>> {
    let t = Rc::new(RefCell::new(TableWidget::new(0, 5).build()));
    t.borrow_mut().set_has_parent();
    t.borrow_mut().set_horizontal_header_labels(&[
        "Version",
        "Noyau",
        "SHA256",
        "Mo",
        "Statut",
    ]);
    t.borrow_mut().set_column_width(0, 100);
    t.borrow_mut().set_column_width(1, 150);
    t.borrow_mut().set_column_width(2, 150);
    t.borrow_mut().set_column_width(3, 60);
    t.borrow_mut().set_column_width(4, 120);
    let _ = kind;
    t
}

/// Construit la zone « Actions » d'un onglet (Actualiser / Installer /
/// Supprimer). Chaque bouton opère sur la table et la liste de kernels du
/// noyau correspondant (`tag`), et partage la liste « installés », le statut
/// et le journal.
fn build_actions(
    tag: u8,
    kind: KernelKind,
    table: &Rc<RefCell<TableWidget>>,
    kernels: &Rc<RefCell<Vec<KernelRelease>>>,
    parent: &dyn AsWidget,
    shared: &SharedUi,
) -> HBoxLayout {
    let mut actions_l = HBoxLayout::with_parent(parent);
    actions_l.set_spacing(10);

    // ---- Actualiser --------------------------------------------------------
    {
        let tx = shared.tx.clone();
        let status = shared.status.clone();
        let logw = shared.logw.clone();
        let label = kind.label().to_string();
        let refresh = PushButton::new(&format!("Actualiser — {}", label))
            .on_clicked(move || {
                set_status(&status, &format!("Interrogation du dépôt {}…", label));
                append_log(&logw, &format!("Requête vers le dépôt {}…", label));
                let tw = tx.clone();
                std::thread::spawn(move || match fetch_latest_kernels(kind) {
                    Ok(k) => {
                        let inst = list_installed();
                        let _ =
                            tw.send(WorkerMsg::Fetch { tag, kernels: k, installed: inst });
                    }
                    Err(e) => {
                        let _ = tw.send(WorkerMsg::Done { ok: false, message: e });
                    }
                });
            })
            .parent(parent)
            .build();
        actions_l.add(refresh);
    }

    // ---- Installer ---------------------------------------------------------
    {
        let tx = shared.tx.clone();
        let status = shared.status.clone();
        let logw = shared.logw.clone();
        let table = table.clone();
        let kernels = kernels.clone();
        let label = kind.label().to_string();
        let install_btn = PushButton::new(&format!("Installer — {}", label))
            .on_clicked(move || {
                let row = table.borrow().current_row();
                if row < 0 {
                    set_status(
                        &status,
                        &format!(
                            "Sélectionnez d'abord une version {} ci-dessus.",
                            label
                        ),
                    );
                    return;
                }
                let k = {
                    let ks = kernels.borrow();
                    let i = row as usize;
                    if i >= ks.len() {
                        set_status(
                            &status,
                            "Liste vide — cliquez d'abord sur « Actualiser ».",
                        );
                        return;
                    }
                    ks[i].clone()
                };
                let kv = k.kernel_version.clone();
                let pw = match sysutil::ask_root_password() {
                    Some(p) => p,
                    None => {
                        set_status(&status, "Annulé (pas de mot de passe).");
                        return;
                    }
                };
                set_status(
                    &status,
                    &format!("Installation de {} ({}) …", kv, label),
                );
                append_log(
                    &logw,
                    &format!("Installation de {} ({}) démarrée.", kv, label),
                );

                let tw = tx.clone();
                std::thread::spawn(move || {
                    let tx2 = tw.clone();
                    let step = |i: u32, t: &str| {
                        let _ = tx2.send(WorkerMsg::Progress {
                            pct: ((i * 100) / 6) as i32,
                            text: t.to_string(),
                        });
                        let _ = tx2.send(WorkerMsg::Log(t.to_string()));
                    };
                    let path = match download_kernel(&k, &|recv, tot| {
                        if tot > 0 {
                            let _ = tx2.send(WorkerMsg::Progress {
                                pct: ((recv * 33 / tot) as i32).min(33),
                                text: format!(
                                    "Téléchargement {} / {} Mo",
                                    recv / 1_048_576,
                                    tot / 1_048_576
                                ),
                            });
                        }
                    }) {
                        Ok(p) => p,
                        Err(e) => {
                            let _ = tx2.send(WorkerMsg::Done { ok: false, message: e });
                            return;
                        }
                    };
                    match install::install_kernel(&pw, &path, &k.kernel_version, k.kind, &step) {
                        Ok(entry) => {
                            let _ = tx2.send(WorkerMsg::Done {
                                ok: true,
                                message: format!(
                                    "Noyau {} ({}) installé ✓\n\nEntrée de boot : {}\n\nRedémarrez pour l'utiliser.",
                                    k.kernel_version, k.kind.label(), entry
                                ),
                            });
                        }
                        Err(e) => {
                            let _ = tx2.send(WorkerMsg::Done { ok: false, message: e });
                        }
                    }
                });
            })
            .parent(parent)
            .build();
        actions_l.add(install_btn);
    }

    // ---- Supprimer ---------------------------------------------------------
    {
        let tx = shared.tx.clone();
        let status = shared.status.clone();
        let logw = shared.logw.clone();
        let label = kind.label().to_string();
        let clean_btn = PushButton::new(&format!("Supprimer {}…", label))
            .on_clicked(move || {
                let inst = list_installed();
                // Noyaux installés de ce type uniquement.
                let mut choices: Vec<String> = Vec::new();
                let mut running = String::from("inconnu");
                for k in &inst {
                    if k.kind != kind {
                        continue;
                    }
                    if k.running {
                        running = k.version.clone();
                    } else {
                        choices.push(k.version.clone());
                    }
                }
                if choices.is_empty() {
                    qtrs::MessageBox::about(
                        None,
                        "Rien à supprimer",
                        &format!(
                            "Aucun noyau {} ancien à supprimer (le noyau en cours ne peut pas être retiré).",
                            label
                        ),
                    );
                    return;
                }
                let items: Vec<&str> = choices.iter().map(|s| s.as_str()).collect();
                let choice = qtrs::get_item(
                    None,
                    &format!("Supprimer un noyau {}", label),
                    &format!(
                        "Noyau en cours : {} (protégé).\nChoisissez le noyau {} à supprimer :",
                        running, label
                    ),
                    &items,
                    0,
                    false,
                );
                if choice.is_empty() {
                    return;
                }
                let pw = match sysutil::ask_root_password() {
                    Some(p) => p,
                    None => {
                        set_status(&status, "Annulé (pas de mot de passe).");
                        return;
                    }
                };
                set_status(
                    &status,
                    &format!("Suppression de {} ({}) …", choice, label),
                );
                append_log(&logw, &format!("Suppression de {}.", choice));
                let ver_choice = choice.clone();
                let tw = tx.clone();
                std::thread::spawn(move || match remove_kernel(&pw, &ver_choice) {
                    Ok(()) => {
                        let _ = tw.send(WorkerMsg::Done {
                            ok: true,
                            message: format!("Noyau {} supprimé ✓", ver_choice),
                        });
                    }
                    Err(e) => {
                        let _ = tw.send(WorkerMsg::Done { ok: false, message: e });
                    }
                });
            })
            .parent(parent)
            .build();
        actions_l.add(clean_btn);
    }

    actions_l
}

// ------------------------------------------------------------------
// Interface
// ------------------------------------------------------------------
struct Handles {
    /// Table par noyau (index = tag : 0=CachyOS, 1=Zen, 2=Liquorix).
    tables: [Rc<RefCell<TableWidget>>; 3],
    /// Liste de kernels par noyau (même index).
    kernel_lists: [Rc<RefCell<Vec<KernelRelease>>>; 3],
    list: Rc<RefCell<ListWidget>>,
    bar: Rc<RefCell<ProgressBar>>,
    /// Jauge « espace libre /boot » (barre de progression, valeur en %).
    /// Retenu en vie (le `Drop` du wrapper ne supprime pas le C++ car
    /// `set_has_parent`, mais on le garde pour éviter tout doublon).
    #[allow(dead_code)]
    free_bar: Rc<RefCell<ProgressBar>>,
    status: Rc<RefCell<Label>>,
    logw: Rc<RefCell<PlainTextEdit>>,
    /// CRITIQUE : les layouts qtrs sont supprimés en C++ dans leur `Drop`.
    /// S'ils tombaient en fin de `build_ui`, les widgets qu'ils positionnent
    /// deviendraient orphelins et l'interface s'afficherait « vide ». On les
    /// garde donc en vie ici, aussi longtemps que `Handles` (retenu dans `main`).
    #[allow(dead_code)]
    layouts: Layouts,
    /// CRITIQUE : `MenuBar` détient les `Menu` qui détiennent les closures
    /// des entrées de menu via leurs `signal_handles`. Si la barre était
    /// droppée en fin de `build_ui`, les closures seraient libérées et un clic
    /// invoquerait un trampoline sur un token libéré → segfault.
    #[allow(dead_code)]
    menubar: MenuBar,
    /// Conteneur du contenu (sans parent) placé dans `central_scroll`.
    #[allow(dead_code)]
    central: Widget,
    /// Enveloppe défilante de la zone centrale.
    #[allow(dead_code)]
    central_scroll: ScrollArea,
    /// Onglet « Avancé » (compilation d'un noyau kernel.org).
    adv: AdvHandles,
    /// Pointeur du `QTabWidget` : sa hauteur est ajustée à la page courante
    /// (sinon les onglets courts héritent de la hauteur de l'onglet Avancé et
    /// affichent un grand vide).
    tabs: *mut qtrs::ffi::QTabWidget,
}

#[allow(dead_code)]
struct Layouts {
    root: VBoxLayout,
    avail: [VBoxLayout; 3],
    /// Layouts des boutons d'actions de chaque onglet (retenus en vie — voir
    /// `build_tab_page`).
    actions: [HBoxLayout; 3],
    inst: VBoxLayout,
    boot: VBoxLayout,
    boot_row: HBoxLayout,
    bar_row: HBoxLayout,
    /// Ligne de la jauge « espace libre /boot » (label + barre).
    free_row: HBoxLayout,
    /// Layouts de l'onglet « Avancé ».
    adv: AdvLayouts,
    /// Layouts de l'onglet « systemd-boot » (page + groupe).
    boot_page: VBoxLayout,
}

/// Construit une page d'onglet pour un noyau donnée : la table des versions
/// disponibles dans un ScrollArea, **suivie des boutons d'actions** (Actualiser
/// / Installer / Supprimer) propres à ce noyau. Retourne le widget page, son
/// layout ET le layout des actions (à retenir en vie, voir `Layouts` — un
/// layout dropé supprimerait son C++ et orphelinerait les boutons).
fn build_tab_page(
    kind: KernelKind,
    table: &Rc<RefCell<TableWidget>>,
    kernels: &Rc<RefCell<Vec<KernelRelease>>>,
    tag: u8,
    shared: &SharedUi,
    wp: &dyn AsWidget,
) -> (Widget, VBoxLayout, HBoxLayout) {
    let page = Widget::new().build();
    let mut l = VBoxLayout::with_parent(&page);
    l.set_contents_margins(8, 8, 8, 8);
    l.set_spacing(6);
    l.add(
        Label::new(&format!("Versions disponibles — noyaux {}", kind.label())).build(),
    );
    // Hauteur fixe : 3 lignes visibles + l'en-tête. Sans cela la table
    // s'étire et repousse les boutons d'action hors de l'onglet.
    let table_sa = wrap_scroll(table, wp);
    unsafe { qtrs::ffi::QWidget_setFixedHeight(table_sa.widget_ptr(), 132) };
    l.add(table_sa);
    // Les actions vivent DANS l'onglet (et non dans une zone commune en bas)
    // : chaque noyau a ses 3 boutons, on gagne de la place verticale.
    // Un layout n'est pas un widget (pas `AsWidget`) : on l'installe sur un
    // widget-conteneur parenté par la page, puis on ajoute ce widget. Le
    // layout lui-même est retenu dans `Layouts` (sinon son `Drop` supprimerait
    // le C++ et orphelinerait les boutons).
    let mut actions_row_w = Widget::new().parent(&page).build();
    let actions_l = build_actions(tag, kind, table, kernels, &actions_row_w, shared);
    actions_row_w.set_layout(&actions_l);
    l.add(actions_row_w);
    // Sans ce ressort, la place restante se répartit ENTRE la table (hauteur
    // fixe) et les boutons : gros vide au milieu et boutons collés en bas.
    l.add_spacer(Spacer::vertical_expanding());
    (page, l, actions_l)
}


// ------------------------------------------------------------------
// Onglet « Avancé » : noyau compilé depuis kernel.org
// ------------------------------------------------------------------

/// Réglages lus dans les widgets de l'onglet au moment du clic.
struct AdvInputs {
    /// Version kernel.org sélectionnée (`6.12.118`), vide si rien n'est choisi.
    version: String,
    opts: BuildOptions,
}

/// Contrôles de l'onglet « Avancé ».
///
/// Les widgets eux-mêmes vivent dans les layouts retenus par [`Layouts`] ; on
/// ne garde ici que des pointeurs bruts pour les lire au moment du clic
/// (`QComboBox_currentText` / `QCheckBox_isChecked` / `QLineEdit_text`
/// renvoient l'état VIVANT, contrairement à `LineEdit::text()` qui est un
/// cache qtrs).
struct AdvHandles {
    /// Versions kernel.org de la liste déroulante (le libellé fait foi).
    versions: Rc<RefCell<Vec<source::OrgRelease>>>,
    /// Travail préparé (sources extraites + `.config` généré).
    prepared: Rc<RefCell<Option<Prepared>>>,
    /// Vrai pendant une préparation ou une compilation : garde anti double-clic.
    /// (`Arc<AtomicBool>` et non `Rc<Cell<bool>>` : le worker le remet à faux
    /// depuis son propre fil.)
    busy: Arc<AtomicBool>,
    /// Drapeau partagé avec le worker, armé par « Annuler ».
    cancel: Arc<AtomicBool>,
    combo: *mut qtrs::ffi::QComboBox,
    base: *mut qtrs::ffi::QComboBox,
    preempt: *mut qtrs::ffi::QComboBox,
    march: *mut qtrs::ffi::QCheckBox,
    suffix: *mut qtrs::ffi::QLineEdit,
    jobs: *mut qtrs::ffi::QSpinBox,
    status: *mut qtrs::ffi::QLabel,
    /// Note affichée sous la liste « Base du .config » : elle ne décrit que la
    /// base sélectionnée (mise à jour par la pompe, sans signal Qt).
    base_note: *mut qtrs::ffi::QLabel,
}

/// Explication de la base de `.config` sélectionnée (une seule à la fois).
///
/// Chaque ligne fait moins de 60 caractères : qtrs n'expose pas le retour à la
/// ligne des QLabel, et une ligne trop longue est COUPÉE (pas repliée) quand la
/// fenêtre est étroite ou la police grande. Constaté sur le PC de test.
fn base_note_for(label: &str) -> &'static str {
    match config_base_from_label(label) {
        ConfigBase::Current => {
            "Reprend la configuration du noyau Solus actuel :\n\
             tous les pilotes, comme la distribution.\n\
             Exige /proc/config.gz ou /boot/config-*."
        }
        ConfigBase::Defconfig => {
            "Tous les pilotes du noyau : 30 à 60 min, 12 Go.\n\
             L'appli réactive zram, codecs audio, pare-feu…\n\
             mais reste plus pauvre qu'une config Solus."
        }
        ConfigBase::LocalMod => {
            "Seulement le matériel chargé maintenant :\n\
             compilation courte, mais supprime les pilotes\n\
             que Solus compile en dur (btrfs, zram, son)."
        }
    }
}

struct AdvLayouts {
    #[allow(dead_code)]
    page: VBoxLayout,
    #[allow(dead_code)]
    opts: VBoxLayout,
    #[allow(dead_code)]
    row: HBoxLayout,
    #[allow(dead_code)]
    tools: HBoxLayout,
    #[allow(dead_code)]
    actions: HBoxLayout,
}

/// Traduit un libellé de la liste « Base du .config » en valeur.
fn config_base_from_label(s: &str) -> ConfigBase {
    for b in [
        ConfigBase::Current,
        ConfigBase::LocalMod,
        ConfigBase::Defconfig,
    ] {
        if b.label() == s {
            return b;
        }
    }
    ConfigBase::LocalMod
}

/// Traduit un libellé de la liste « Préemption » en valeur.
fn preempt_from_label(s: &str) -> Preempt {
    for p in [
        Preempt::Keep,
        Preempt::None,
        Preempt::Voluntary,
        Preempt::Full,
        Preempt::Lazy,
    ] {
        if p.label() == s {
            return p;
        }
    }
    Preempt::Keep
}

/// Version nue depuis le libellé d'une entrée (`6.12.118 — longterm (LTS)`).
fn version_from_label(s: &str) -> String {
    s.split(" — ").next().unwrap_or("").trim().to_string()
}

/// Récapitulatif lisible des options retenues (affiché avant de compiler).
fn recap(inputs: &AdvInputs) -> String {
    let mut parts: Vec<String> = vec![
        if inputs.version.is_empty() {
            "version non choisie".to_string()
        } else {
            format!("{}{}", inputs.version, inputs.opts.localversion.trim())
        },
        inputs.opts.base.label().to_string(),
        inputs.opts.preempt.label().to_string(),
    ];
    if inputs.opts.march_native {
        parts.push("KCFLAGS=-march=native".to_string());
    }
    parts.push(format!("make -j{}", inputs.opts.jobs));
    parts.join(" · ")
}

/// Lit les contrôles de l'onglet et construit les options de compilation.
fn read_adv_inputs(h: &AdvHandles) -> AdvInputs {
    use qtrs::ffi;
    let label = unsafe { ffi::QComboBox_currentText(h.combo) };
    let base_label = unsafe { ffi::QComboBox_currentText(h.base) };
    let preempt_label = unsafe { ffi::QComboBox_currentText(h.preempt) };
    let march = unsafe { ffi::QCheckBox_isChecked(h.march) };
    let suffix = unsafe { ffi::QLineEdit_text(h.suffix) };
    let jobs = unsafe { ffi::QSpinBox_value(h.jobs) }.max(1) as u32;
    AdvInputs {
        version: version_from_label(&label),
        opts: BuildOptions {
            base: config_base_from_label(&base_label),
            preempt: preempt_from_label(&preempt_label),
            march_native: march,
            localversion: suffix,
            jobs,
            build_root: build::default_build_root(),
        },
    }
}

/// Hauteur voulue par onglet : les trois onglets « famille » et systemd-boot
/// n'ont besoin que de leur table et de leurs boutons ; l'onglet Avancé porte
/// tout le panneau d'options.
fn tab_height_for(index: i32) -> i32 {
    match index {
        3 => 720, // Avancé (panneau d'options + note de base sur 3 lignes)
        4 => 275, // systemd-boot
        _ => 250, // CachyOS / Zen / Liquorix
    }
}

/// Ajuste la hauteur du `QTabWidget` à la page courante. Sans cela tous les
/// onglets prennent la hauteur du plus grand (Avancé) et laissent un vide.
fn apply_tab_height(tabs: *mut qtrs::ffi::QTabWidget) {
    let idx = unsafe { qtrs::ffi::QTabWidget_currentIndex(tabs) };
    unsafe {
        qtrs::ffi::QWidget_setFixedHeight(
            tabs as *mut qtrs::ffi::QWidget,
            tab_height_for(idx),
        )
    };
}

/// Passe la barre commune en mode « occupé » : plage 0..0, que Qt anime en
/// va-et-vient. À faire au démarrage de toute opération, et à annuler par
/// `set_range(0, 100)` quand elle se termine (le fait la pompe sur `Done`).
fn set_bar_busy(bar: &Rc<RefCell<ProgressBar>>) {
    bar.borrow().set_range(0, 0);
}

/// Confirmation modale Oui/Annuler. Vrai si l'utilisateur confirme.
///
/// Construite **sans parent** : passer la fenêtre principale forcerait un
/// `borrow()` vivant pendant la boucle d'événements imbriquée.
fn confirm(title: &str, text: &str) -> bool {
    let mb = qtrs::MessageBox::new()
        .icon(qtrs::QUESTION)
        .text(title)
        .informative_text(text)
        .window_title("Solus External Kernel Manager")
        .standard_buttons(qtrs::YES | qtrs::CANCEL)
        .default_button(qtrs::YES)
        .build();
    mb.exec() == qtrs::YES
}


/// Lance la récupération de la liste kernel.org en tâche de fond.
/// Utilisé par le bouton « Lister les versions » et par le hook de test
/// `SEKM_AUTOFETCH` (capture d'écran avec la liste remplie).
fn spawn_org_fetch(tx: &mpsc::Sender<WorkerMsg>, logw: &Rc<RefCell<PlainTextEdit>>) {
    append_log(logw, "Lecture de la liste des versions kernel.org…");
    let tw = tx.clone();
    std::thread::spawn(move || match source::fetch_releases() {
        Ok(v) => {
            let _ = tw.send(WorkerMsg::OrgReleases(v));
        }
        Err(e) => {
            let _ = tw.send(WorkerMsg::Done {
                ok: false,
                message: e,
            });
        }
    });
}

/// Construit la page de l'onglet « Avancé ».
fn build_advanced_page(shared: &SharedUi) -> (Widget, AdvLayouts, AdvHandles) {
    use qtrs::ffi;


    let page = Widget::new().build();
    let mut l = VBoxLayout::with_parent(&page);
    l.set_contents_margins(8, 8, 8, 8);
    l.set_spacing(6);

    l.add(
        Label::new("Compiler un noyau depuis kernel.org")
            .parent(&page)
            .build(),
    );
    l.add(
        Label::new(
            "Compilation locale : de 30 min à plusieurs heures, ~12 Go d'espace disque.\n\
             Seul endroit de l'application qui lance un compilateur.",
        )
        .parent(&page)
        .build(),
    );

    // ---- Ligne 1 : version + bouton de rafraîchissement --------------------
    let row_w = Widget::new().parent(&page).build();
    let mut row_l = HBoxLayout::with_parent(&row_w);
    row_l.set_spacing(8);
    row_l.add(Label::new("Version kernel.org :").parent(&row_w).build());
    let combo = ComboBox::new().parent(&row_w).build();
    combo.set_fixed_width(460);
    let combo_ptr = combo.widget_ptr() as *mut ffi::QComboBox;
    row_l.add(combo);
    {
        let tx = shared.tx.clone();
        let logw = shared.logw.clone();
        let refresh = PushButton::new("Lister les versions")
            .on_clicked(move || spawn_org_fetch(&tx, &logw))
            .parent(&row_w)
            .build();
        row_l.add(refresh);
        row_l.add_spacer(Spacer::horizontal_expanding());
    }
    l.add(row_w);

    // ---- Groupe « Options de compilation » --------------------------------
    let opts_box = GroupBox::new("Options de compilation")
        .parent(&page)
        .build();
    // VBoxLayout et non GridLayout : dans une grille à une seule colonne, Qt
    // centre les champs à largeur fixe au milieu du cadre (les libellés
    // n'étaient plus alignés à gauche). Une pile verticale les laisse à gauche.
    let mut g = VBoxLayout::with_parent(&opts_box);
    g.set_spacing(4);

    g.add(
        Label::new("Base du .config :").parent(&opts_box).build(),
    );
    // L'ordre suit la disponibilité réelle : la config du noyau courant d'abord
    // (c'est la seule qui ait donné un noyau complet ET sonore sur le PC de
    // test), puis defconfig, puis localmodconfig.
    let mut base_items: Vec<String> = Vec::new();
    if build::current_config_available() {
        base_items.push(ConfigBase::Current.label().to_string());
    }
    base_items.push(ConfigBase::Defconfig.label().to_string());
    base_items.push(ConfigBase::LocalMod.label().to_string());
    let base_refs: Vec<&str> = base_items.iter().map(|v| v.as_str()).collect();
    let base_cb = ComboBox::new()
        .items(&base_refs)
        .parent(&opts_box)
        .build();
    base_cb.set_fixed_width(400);
    let base_ptr = base_cb.widget_ptr() as *mut ffi::QComboBox;
    g.add(base_cb);
    // Note de la base : vide au départ, remplie par la pompe selon la sélection
    // (`base_note_for`), pour qu'une seule explication soit visible à la fois.
    let base_note = Label::new("").parent(&opts_box).build();
    // Hauteur minimale de 3 lignes : sans elle le QLabel est COMPRIMÉ (texte
    // coupé en hauteur) dès que le contenu de l'onglet dépasse la hauteur du
    // QTabWidget — c'est ce qui rendait les notes illisibles sur un écran à
    // police plus grande.
    unsafe {
        ffi::QWidget_setMinimumSize(base_note.widget_ptr() as *mut ffi::QWidget, 0, 60)
    };
    let base_note_ptr = base_note.widget_ptr() as *mut ffi::QLabel;
    set_label_text(base_note_ptr, base_note_for(&base_refs[0]));
    g.add(base_note);

    g.add(
        Label::new("Préemption :").parent(&opts_box).build(),
    );
    let preempt_cb = ComboBox::new()
        .items(&[
            Preempt::Keep.label(),
            Preempt::None.label(),
            Preempt::Voluntary.label(),
            Preempt::Full.label(),
            Preempt::Lazy.label(),
        ])
        .parent(&opts_box)
        .build();
    preempt_cb.set_fixed_width(320);
    let preempt_ptr = preempt_cb.widget_ptr() as *mut ffi::QComboBox;
    g.add(preempt_cb);
    g.add(
        Label::new(
            "Souvent modifiable au boot sans recompiler\n\
             (PREEMPT_DYNAMIC=y : preempt=lazy|voluntary|full|none).",
        )
        .parent(&opts_box)
        .build(),
    );

    let march_cb = CheckBox::new(
        "-march=native (KCFLAGS) — gain attendu 1 à 3 %, binaire non portable",
    )
    .checked(true)
    .parent(&opts_box)
    .build();
    let march_ptr = march_cb.widget_ptr() as *mut ffi::QCheckBox;
    g.add(march_cb);

    g.add(
        Label::new("Suffixe de version (LOCALVERSION, termine uname -r) :")
            .parent(&opts_box)
            .build(),
    );
    let suffix_edit = LineEdit::new("-kernelorg").parent(&opts_box).build();
    suffix_edit.set_fixed_width(220);
    let suffix_ptr = suffix_edit.widget_ptr() as *mut ffi::QLineEdit;
    g.add(suffix_edit);

    g.add(
        Label::new("Parallélisme de make (-j) :")
            .parent(&opts_box)
            .build(),
    );
    // Plafonné au nombre de cœurs réellement disponibles (ticket #2 : on
    // pouvait demander plus de tâches que de cœurs, ce qui ne fait que
    // saturer la mémoire sans accélérer la compilation).
    let jobs_spin = SpinBox::new()
        .range(1, build::default_jobs() as i32)
        .value(build::default_jobs() as i32)
        .parent(&opts_box)
        .build();
    jobs_spin.set_fixed_width(90);
    let jobs_ptr = jobs_spin.widget_ptr() as *mut ffi::QSpinBox;
    g.add(jobs_spin);

    l.add(opts_box);

    // ---- Statut de l'onglet ----------------------------------------------
    let status_lbl = Label::new("Prêt. Choisissez une version, puis « 1. Préparer ».")
        .parent(&page)
        .build();
    let status_ptr = status_lbl.widget_ptr() as *mut ffi::QLabel;
    l.add(status_lbl);

    let adv = Rc::new(AdvHandles {
        versions: Rc::new(RefCell::new(Vec::new())),
        prepared: Rc::new(RefCell::new(None)),
        busy: Arc::new(AtomicBool::new(false)),
        cancel: Arc::new(AtomicBool::new(false)),
        combo: combo_ptr,
        base: base_ptr,
        preempt: preempt_ptr,
        march: march_ptr,
        suffix: suffix_ptr,
        jobs: jobs_ptr,
        status: status_ptr,
        base_note: base_note_ptr,
    });

    // ---- Boutons ---------------------------------------------------------
    // Rangée « entretien » (outils + nettoyage), affichée AU-DESSUS des étapes.
    let tools_w = Widget::new().parent(&page).build();
    let mut tools_l = HBoxLayout::with_parent(&tools_w);
    tools_l.set_spacing(8);

    let actions_w = Widget::new().parent(&page).build();
    let mut actions_l = HBoxLayout::with_parent(&actions_w);
    actions_l.set_spacing(8);

    // 1. Préparer
    {
        let tx = shared.tx.clone();
        let logw = shared.logw.clone();
        let adv = Rc::clone(&adv);
        let bar = Rc::clone(&shared.bar);
        let btn = PushButton::new("1. Préparer")
            .on_clicked(move || {
                let status = adv.status;
                if adv.busy.load(Ordering::Relaxed) {
                    set_label_text(status, "Un travail est déjà en cours — patientez ou annulez.");
                    return;
                }
                let inputs = read_adv_inputs(&adv);
                if inputs.version.is_empty() {
                    set_label_text(
                        status,
                        "Choisissez d'abord une version (bouton « Lister les versions »).",
                    );
                    return;
                }
                if let Err(e) = build::validate_localversion(&inputs.opts.localversion) {
                    set_label_text(status, &e);
                    return;
                }
                let rel = {
                    let v = adv.versions.borrow();
                    v.iter().find(|r| r.version == inputs.version).cloned()
                };
                let rel = match rel {
                    Some(r) => r,
                    None => {
                        set_label_text(
                            status,
                            &format!(
                                "version {} absente de la liste — relancez « Lister les versions »",
                                inputs.version
                            ),
                        );
                        return;
                    }
                };
                // Une version -rc n'est pas signée par kernel.org : on demande
                // une confirmation explicite, et le journal le dira.
                let allow_unsigned = if rel.rc {
                    let ok = confirm(
                        "Version -rc non signée",
                        &format!(
                            "{} est une pré-version de développement.\n\n\
                             kernel.org ne publie AUCUNE signature PGP pour les -rc : \
                             l'archive ne peut pas être authentifiée.\n\n\
                             Continuer quand même (compilation non vérifiée) ?",
                            rel.version
                        ),
                    );
                    if !ok {
                        set_label_text(status, "Version -rc refusée (aucune signature).");
                        return;
                    }
                    true
                } else {
                    false
                };
                adv.cancel.store(false, Ordering::Relaxed);
                adv.busy.store(true, Ordering::Relaxed);
                set_bar_busy(&bar);
                append_log(&logw, &format!("Préparation — {}", recap(&inputs)));
                set_label_text(status, "Préparation : téléchargement, signature, configuration…");
                append_log(
                    &logw,
                    &format!("Préparation de {} — {}", rel.version, recap(&inputs)),
                );
                let tw = tx.clone();
                let cancel = Arc::clone(&adv.cancel);
                let busy = Arc::clone(&adv.busy);
                let opts = inputs.opts.clone();
                std::thread::spawn(move || {
                    let ui = ChannelUi { tx: tw.clone() };
                    match build::prepare(&rel, &opts, &ui, &cancel, allow_unsigned) {
                        Ok(p) => {
                            let _ = tw.send(WorkerMsg::Prepared(Box::new(p)));
                            let _ = tw.send(WorkerMsg::Done {
                                ok: true,
                                message: "Sources et configuration prêtes.\n\n\
                                          Étapes suivantes :\n\
                                          • « 2. Ouvrir menuconfig » pour ajuster à la main (optionnel)\n\
                                          • « 3. Compiler et installer »"
                                    .to_string(),
                            });
                        }
                        Err(e) => {
                            let _ = tw.send(WorkerMsg::Done { ok: false, message: e });
                        }
                    }
                    busy.store(false, Ordering::Relaxed);
                });
            })
            .parent(&actions_w)
            .build();
        actions_l.add(btn);
    }

    // 2. menuconfig
    {
        let logw = shared.logw.clone();
        let adv = Rc::clone(&adv);
        let btn = PushButton::new("2. Menuconfig")
            .on_clicked(move || {
                let status = adv.status;
                if adv.busy.load(Ordering::Relaxed) {
                    set_label_text(status, "Un travail est en cours — menuconfig attendra.");
                    return;
                }
                let src = { adv.prepared.borrow().as_ref().map(|p| p.src.clone()) };
                let src = match src {
                    Some(s) => s,
                    None => {
                        set_label_text(
                            status,
                            "Cliquez d'abord sur « 1. Préparer » : menuconfig a besoin des sources.",
                        );
                        return;
                    }
                };
                let (bin, args) = match build::menuconfig_command() {
                    Ok(c) => c,
                    Err(e) => {
                        set_label_text(status, &e);
                        append_log(&logw, &format!("ERREUR menuconfig : {}", e));
                        return;
                    }
                };
                match std::process::Command::new(&bin)
                    .args(&args)
                    .current_dir(&src)
                    .spawn()
                {
                    Ok(_) => {
                        if let Some(p) = adv.prepared.borrow_mut().as_mut() {
                            p.manual_edit = true;
                        }
                        append_log(
                            &logw,
                            "menuconfig ouvert. La configuration éditée à la main fera foi : les \
                             options de l'onglet ne seront pas réappliquées.",
                        );
                        set_label_text(
                            status,
                            "menuconfig ouvert — votre configuration manuelle sera conservée.",
                        );
                    }
                    Err(e) => {
                        append_log(&logw, &format!("ERREUR lancement de {} : {}", bin, e));
                        set_label_text(status, &format!("Impossible d'ouvrir {} : {}", bin, e));
                    }
                }
            })
            .parent(&actions_w)
            .build();
        actions_l.add(btn);
    }

    // 3. Compiler et installer
    {
        let tx = shared.tx.clone();
        let logw = shared.logw.clone();
        let adv = Rc::clone(&adv);
        let bar = Rc::clone(&shared.bar);
        let btn = PushButton::new("3. Compiler et installer")
            .on_clicked(move || {
                let status = adv.status;
                if adv.busy.load(Ordering::Relaxed) {
                    set_label_text(status, "Une opération est déjà en cours.");
                    return;
                }
                // On sort du borrow AVANT tout dialogue modal.
                let prepared = { adv.prepared.borrow().clone() };
                let p = match prepared {
                    Some(p) => p,
                    None => {
                        set_label_text(
                            status,
                            "Cliquez d'abord sur « 1. Préparer » (archive, signature, configuration).",
                        );
                        return;
                    }
                };
                // Options FIGÉES à la préparation : les widgets ont pu changer
                // depuis, et c'est la configuration préparée qui sera compilée.
                let opts = p.opts.clone();
                let text = format!(
                    "Noyau à compiler : {}\n\nRépertoire de travail :\n{}\n\n\
                     Étape longue (30 min à plusieurs heures). La compilation peut être \
                     annulée, mais repartira de zéro.\n\n\
                     Configuration : {}",
                    p.kver,
                    p.src.display(),
                    if p.manual_edit {
                        "éditée à la main (menuconfig) — les options de l'onglet ne seront pas réappliquées"
                            .to_string()
                    } else {
                        recap(&AdvInputs {
                            version: p.version.clone(),
                            opts: opts.clone(),
                        })
                    }
                );
                // État préparé périmé (sources nettoyées, disque déplacé,
                // redémarrage) : `make` renverrait « No rule to make target »
                // sans que l'utilisateur voie pourquoi.
                if !p.src.join("Makefile").exists() || !p.src.join(".config").exists() {
                    set_label_text(
                        status,
                        "Les sources préparées ne sont plus présentes (nettoyées ou déplacées) : \
                         relancez « 1. Préparer ».",
                    );
                    return;
                }
                if !confirm("Compiler et installer ce noyau ?", &text) {
                    set_label_text(status, "Compilation annulée avant démarrage.");
                    return;
                }
                let pw = match sysutil::ask_root_password() {
                    Some(p) => p,
                    None => {
                        set_label_text(status, "Annulé (pas de mot de passe root).");
                        return;
                    }
                };
                adv.cancel.store(false, Ordering::Relaxed);
                adv.busy.store(true, Ordering::Relaxed);
                set_bar_busy(&bar);
                set_label_text(status, "Compilation en cours…");
                append_log(
                    &logw,
                    &format!("Compilation de {} démarrée (journal en direct).", p.kver),
                );
                let tw = tx.clone();
                let cancel = Arc::clone(&adv.cancel);
                let busy = Arc::clone(&adv.busy);
                std::thread::spawn(move || {
                    let ui = ChannelUi { tx: tw.clone() };
                    match build::build_and_install(&pw, &p, &ui, &cancel) {
                        Ok((kver, entry)) => {
                            let _ = tw.send(WorkerMsg::Done {
                                ok: true,
                                message: format!(
                                    "Noyau {} compilé et installé ✓\n\nEntrée de boot : {}\n\n\
                                     Redémarrez pour l'utiliser.",
                                    kver, entry
                                ),
                            });
                        }
                        Err(e) => {
                            let _ = tw.send(WorkerMsg::Done { ok: false, message: e });
                        }
                    }
                    busy.store(false, Ordering::Relaxed);
                });
            })
            .parent(&actions_w)
            .build();
        actions_l.add(btn);
    }

    // Annuler
    {
        let logw = shared.logw.clone();
        let adv = Rc::clone(&adv);
        let btn = PushButton::new("Annuler")
            .on_clicked(move || {
                adv.cancel.store(true, Ordering::Relaxed);
                append_log(&logw, "Annulation demandée : arrêt du processus en cours…");
                set_label_text(adv.status, "Annulation demandée…");
            })
            .parent(&actions_w)
            .build();
        actions_l.add(btn);
    }

    // Installer les outils de compilation (prérequis de l'étape 1/3)
    {
        let tx = shared.tx.clone();
        let logw = shared.logw.clone();
        let adv = Rc::clone(&adv);
        let bar = Rc::clone(&shared.bar);
        let btn = PushButton::new("Installer les outils…")
            .on_clicked(move || {
                let status = adv.status;
                if adv.busy.load(Ordering::Relaxed) {
                    set_label_text(status, "Une opération est déjà en cours.");
                    return;
                }
                let root = build::default_build_root();
                let pf = build::preflight(&root, true);
                if pf.missing.is_empty() {
                    set_label_text(
                        status,
                        "Tous les outils de compilation sont déjà présents.",
                    );
                    append_log(&logw, "Pré-vol : aucun outil manquant.");
                    return;
                }
                if !build::eopkg_available() {
                    set_label_text(
                        status,
                        "`eopkg` introuvable : installez les outils à la main (voir le journal).",
                    );
                    append_log(
                        &logw,
                        &format!(
                            "Outils manquants : {}. `eopkg` absent : installation automatique \
                             impossible, paquets équivalents à installer manuellement : {}",
                            pf.missing.join(", "),
                            build::TOOLCHAIN_PACKAGES.join(" ")
                        ),
                    );
                    return;
                }
                let text = format!(
                    "Outils manquants : {}\n\nSeront installés avec eopkg :\n{}\n\n\
                     La liste est celle que la compilation d'un noyau a réellement exigée \
                     (system.devel fournit gcc, make, binutils et glibc-devel).",
                    pf.missing.join(", "),
                    build::TOOLCHAIN_PACKAGES.join(", ")
                );
                if !confirm("Installer les outils de compilation ?", &text) {
                    set_label_text(status, "Installation des outils annulée.");
                    return;
                }
                let pw = match sysutil::ask_root_password() {
                    Some(p) => p,
                    None => {
                        set_label_text(status, "Annulé (pas de mot de passe root).");
                        return;
                    }
                };
                adv.cancel.store(false, Ordering::Relaxed);
                adv.busy.store(true, Ordering::Relaxed);
                set_bar_busy(&bar);
                set_label_text(status, "Installation des outils de compilation…");
                append_log(&logw, "Installation des outils de compilation via eopkg…");
                let tw = tx.clone();
                let cancel = Arc::clone(&adv.cancel);
                let busy = Arc::clone(&adv.busy);
                std::thread::spawn(move || {
                    let ui = ChannelUi { tx: tw.clone() };
                    match build::install_toolchain(&pw, &root, &ui, &cancel) {
                        Ok(()) => {
                            let _ = tw.send(WorkerMsg::Done {
                                ok: true,
                                message: "Outils de compilation installés.\n\n\
                                          Vous pouvez relancer « 1. Préparer »."
                                    .to_string(),
                            });
                        }
                        Err(e) => {
                            let _ = tw.send(WorkerMsg::Done { ok: false, message: e });
                        }
                    }
                    busy.store(false, Ordering::Relaxed);
                });
            })
            .parent(&tools_w)
            .build();
        tools_l.add(btn);
    }

    // Désinstaller les outils de compilation
    {
        let tx = shared.tx.clone();
        let logw = shared.logw.clone();
        let adv = Rc::clone(&adv);
        let bar = Rc::clone(&shared.bar);
        let btn = PushButton::new("Désinstaller les outils…")
            .on_clicked(move || {
                let status = adv.status;
                if adv.busy.load(Ordering::Relaxed) {
                    set_label_text(status, "Une opération est déjà en cours.");
                    return;
                }
                if !build::eopkg_available() {
                    set_label_text(status, "`eopkg` introuvable : rien à désinstaller.");
                    return;
                }
                let text = "Seront retirés (s'ils sont installés) :\n\
                            gcc, make, binutils, glibc-devel, linux-headers, bison, flex, m4,\n\
                            diffutils, elfutils-devel, ncurses-devel, bc, perl, dracut, gnupg.\n\n\
                            eopkg REFUSE de retirer un paquet dont dépend un autre paquet Solus :\n\
                            ceux-là resteront, et la liste finale sera affichée dans le journal.\n\n\
                            Cela touche tout le système, pas seulement cette application.";
                if !confirm("Désinstaller les outils de compilation ?", text) {
                    set_label_text(status, "Désinstallation annulée.");
                    return;
                }
                let pw = match sysutil::ask_root_password() {
                    Some(p) => p,
                    None => {
                        set_label_text(status, "Annulé (pas de mot de passe root).");
                        return;
                    }
                };
                adv.cancel.store(false, Ordering::Relaxed);
                adv.busy.store(true, Ordering::Relaxed);
                set_bar_busy(&bar);
                set_label_text(status, "Désinstallation des outils…");
                append_log(&logw, "Désinstallation des outils de compilation (eopkg)…");
                let tw = tx.clone();
                let cancel = Arc::clone(&adv.cancel);
                let busy = Arc::clone(&adv.busy);
                std::thread::spawn(move || {
                    let ui = ChannelUi { tx: tw.clone() };
                    let _ = build::uninstall_toolchain(&pw, &ui, &cancel);
                    let _ = tw.send(WorkerMsg::Done {
                        ok: true,
                        message: "Désinstallation terminée.\n\nLa liste de ce qui a été conservé \
                                  (requis par d'autres paquets) est dans le journal."
                            .to_string(),
                    });
                    busy.store(false, Ordering::Relaxed);
                });
            })
            .parent(&tools_w)
            .build();
        tools_l.add(btn);
    }

    // Supprimer un noyau compilé (indispensable : sans cela, un noyau
    // kernel.org cassé ne pouvait être retiré que depuis un terminal, et le
    // garde-fou « déjà installé » bloquait toute recompilation).
    {
        let tx = shared.tx.clone();
        let logw = shared.logw.clone();
        let adv = Rc::clone(&adv);
        let btn = PushButton::new("Supprimer le noyau…")
            .on_clicked(move || {
                let status = adv.status;
                if adv.busy.load(Ordering::Relaxed) {
                    set_label_text(status, "Une opération est en cours.");
                    return;
                }
                let mut choices: Vec<String> = Vec::new();
                let mut running = String::from("inconnu");
                for k in list_installed() {
                    if k.kind != KernelKind::KernelOrg {
                        continue;
                    }
                    if k.running {
                        running = k.version.clone();
                    } else {
                        choices.push(k.version);
                    }
                }
                if choices.is_empty() {
                    qtrs::MessageBox::about(
                        None,
                        "Rien à supprimer",
                        "Aucun noyau compilé à retirer (le noyau en cours ne peut pas l'être).",
                    );
                    return;
                }
                let items: Vec<&str> = choices.iter().map(|s| s.as_str()).collect();
                let choice = qtrs::get_item(
                    None,
                    "Supprimer un noyau compilé",
                    &format!(
                        "Noyau en cours : {} (protégé).\nChoisissez le noyau à supprimer :",
                        running
                    ),
                    &items,
                    0,
                    false,
                );
                if choice.is_empty() {
                    return;
                }
                let pw = match sysutil::ask_root_password() {
                    Some(p) => p,
                    None => {
                        set_label_text(status, "Annulé (pas de mot de passe root).");
                        return;
                    }
                };
                set_label_text(status, &format!("Suppression de {} …", choice));
                append_log(&logw, &format!("Suppression du noyau compilé {}.", choice));
                let ver = choice.clone();
                let tw = tx.clone();
                std::thread::spawn(move || match remove_kernel(&pw, &ver) {
                    Ok(()) => {
                        let _ = tw.send(WorkerMsg::Done {
                            ok: true,
                            message: format!("Noyau {} supprimé ✓", ver),
                        });
                    }
                    Err(e) => {
                        let _ = tw.send(WorkerMsg::Done { ok: false, message: e });
                    }
                });
            })
            .parent(&actions_w)
            .build();
        actions_l.add(btn);
    }

    // Nettoyer
    {
        let logw = shared.logw.clone();
        let adv = Rc::clone(&adv);
        let btn = PushButton::new("Nettoyer…")
            .on_clicked(move || {
                let status = adv.status;
                if adv.busy.load(Ordering::Relaxed) {
                    set_label_text(status, "Nettoyage impossible pendant un travail.");
                    return;
                }
                let version = { adv.prepared.borrow().as_ref().map(|p| p.version.clone()) };
                let version = match version {
                    Some(v) => v,
                    None => {
                        set_label_text(status, "Aucune source préparée à nettoyer.");
                        return;
                    }
                };
                if !confirm(
                    "Supprimer les sources préparées ?",
                    &format!(
                        "Le répertoire de travail de {} (sources + objets) sera supprimé. \
                         Un noyau déjà installé n'est pas touché.",
                        version
                    ),
                ) {
                    return;
                }
                match build::clean_sources(&build::default_build_root(), &version) {
                    Ok(freed) => {
                        *adv.prepared.borrow_mut() = None;
                        append_log(
                            &logw,
                            &format!(
                                "Sources de {} supprimées ({} libérés).",
                                version,
                                fmt_bytes(freed)
                            ),
                        );
                        set_label_text(status, "Sources supprimées.");
                    }
                    Err(e) => {
                        append_log(&logw, &format!("ERREUR nettoyage : {}", e));
                        set_label_text(status, &format!("Échec du nettoyage : {}", e));
                    }
                }
            })
            .parent(&tools_w)
            .build();
        tools_l.add(btn);
    }

    tools_l.add_spacer(Spacer::horizontal_expanding());
    actions_l.add_spacer(Spacer::horizontal_expanding());
    // Étapes de préparation/compilation d'abord, outils et nettoyage ensuite.
    l.add(actions_w);
    l.add(tools_w);

    let layouts = AdvLayouts {
        page: l,
        opts: g,
        row: row_l,
        tools: tools_l,
        actions: actions_l,
    };
    let handles = AdvHandles {
        versions: Rc::clone(&adv.versions),
        prepared: Rc::clone(&adv.prepared),
        busy: Arc::clone(&adv.busy),
        cancel: Arc::clone(&adv.cancel),
        combo: adv.combo,
        base: adv.base,
        preempt: adv.preempt,
        march: adv.march,
        suffix: adv.suffix,
        jobs: adv.jobs,
        status: adv.status,
        base_note: adv.base_note,
    };
    (page, layouts, handles)
}

fn build_ui(window: &Rc<RefCell<MainWindow>>, tx: mpsc::Sender<WorkerMsg>) -> Handles {
    // ---- Widgets communs (en Rc, sans parent) ------------------------------
    let list = Rc::new(RefCell::new(ListWidget::new().build()));
    let bar = Rc::new(RefCell::new(
        ProgressBar::new().range(0, 100).value(0).format("%p %").build(),
    ));
    let status = Rc::new(RefCell::new(
        Label::new("En attente — cliquez sur « Actualiser » dans un onglet.").build(),
    ));
    let logw = Rc::new(RefCell::new(PlainTextEdit::new().build()));

    // Widgets « noyau » (par onglet).
    let kinds: [KernelKind; 3] = [
        KernelKind::Cachyos,
        KernelKind::Zen,
        KernelKind::Liquorix,
    ];
    let tables_arr: [Rc<RefCell<TableWidget>>; 3] = [
        new_table_for(KernelKind::Cachyos),
        new_table_for(KernelKind::Zen),
        new_table_for(KernelKind::Liquorix),
    ];
    let kernel_lists: [Rc<RefCell<Vec<KernelRelease>>>; 3] = [
        Rc::new(RefCell::new(Vec::new())),
        Rc::new(RefCell::new(Vec::new())),
        Rc::new(RefCell::new(Vec::new())),
    ];

    list.borrow_mut().set_has_parent();
    bar.borrow_mut().set_has_parent();
    status.borrow_mut().set_has_parent();
    logw.borrow_mut().set_has_parent();
    logw.borrow_mut().set_read_only(true);
    logw.borrow_mut().set_placeholder_text("Journal des opérations…");

    let w = window.borrow();
    let wp: &dyn AsWidget = &*w;

    let shared = SharedUi {
        tx: tx.clone(),
        status: status.clone(),
        logw: logw.clone(),
        bar: bar.clone(),
    };

    // ---- Menu ---------------------------------------------------------------
    let logw_menu = logw.clone();
    let file_menu = Menu::new("Fichier")
        .action("Afficher le journal", move || {
            let body = logw_menu.borrow().plain_text();
            sysutil::show_text_window(
                "Journal des opérations",
                if body.trim().is_empty() {
                    "(le journal est vide pour le moment)"
                } else {
                    &body
                },
            );
        })
        .action("Quitter", || std::process::exit(0))
        .parent(wp)
        .build();
    let version = env!("CARGO_PKG_VERSION");
    let help_menu = Menu::new("?")
        .action("À propos", move || {
            qtrs::MessageBox::about(
                None,
                "À propos — Solus External Kernel Manager",
                &format!(
                    "Solus External Kernel Manager — version {}\n\n\
                     Gestionnaire des noyaux externes (CachyOS, Zen, Liquorix) pour Solus, \
                     et compilation d'un noyau depuis kernel.org.\n\n\
                     • Trois onglets, un par famille de noyaux\n\
                     • Onglet « Avancé » : compilation depuis kernel.org (signature PGP)\n\
                     • Détecte les nouvelles versions dans chaque dépôt pacman\n\
                     • Les installe (vmlinuz, initrd dracut, entrée systemd-boot)\n\
                     • Supprime les noyaux anciens (jamais le noyau en cours)\n\n\
                     /boot est monté via `clr-boot-manager mount-boot`.",
                    version
                ),
            )
        })
        .parent(wp)
        .build();
    let menubar = MenuBar::new()
        .add_menu(file_menu)
        .add_menu(help_menu)
        .parent(wp)
        .build();
    w.set_menu_bar(&menubar);

    // ---- Widget central + layout racine ------------------------------------
    let central = Widget::new().build();
    let mut root = VBoxLayout::with_parent(&central);
    root.set_contents_margins(12, 12, 12, 12);
    root.set_spacing(10);

    let title = Label::new("Noyaux externes — Solus").parent(wp).build();
    root.add(title);

    // ---- Onglets (un par noyau) --------------------------------------------
    // Le TabWidget est parenté (Qt gère son lifetime C++) puis transféré au
    // layout `root` via `add` ; on le conserve aussi dans `Handles` (inutile
    // mais légal), et `Layouts` retient chaque layout pour qu'aucun C++ ne soit
    // libéré prématurément en fin de fonction.
    let mut tabs = TabWidget::new()
        .on_current_changed({
            let logw = logw.clone();
            move |idx| {
                append_log(&logw, &format!("Onglet sélectionné : {}", idx));
            }
        })
        .parent(wp)
        .build();
    let mut avail_l_res = [VBoxLayout::new(), VBoxLayout::new(), VBoxLayout::new()];
    let mut actions_l_res = [
        HBoxLayout::new(),
        HBoxLayout::new(),
        HBoxLayout::new(),
    ];
    for i in 0..3 {
        let (page, l, al) =
            build_tab_page(kinds[i], &tables_arr[i], &kernel_lists[i], i as u8, &shared, wp);
        avail_l_res[i] = l;
        actions_l_res[i] = al;
        tabs.add_tab(Box::new(page), kinds[i].label());
    }
    // Onglet « Avancé » : compilation d'un noyau kernel.org. Ce n'est pas une
    // famille de noyaux (pas de dépôt, pas de table de versions) : un
    // assistant séparé, ajouté après les trois onglets pacman.
    let (adv_page, adv_layouts, adv_handles) = build_advanced_page(&shared);
    tabs.add_tab(Box::new(adv_page), "Avancé");
    // ---- Onglet « systemd-boot » ------------------------------------------
    let boot_page = Widget::new().build();
    let mut boot_page_l = VBoxLayout::with_parent(&boot_page);
    boot_page_l.set_contents_margins(8, 8, 8, 8);
    boot_page_l.set_spacing(8);
    // Onglet « systemd-boot » : tous les réglages de démarrage, sortis
    // de la zone commune pour ne plus encombrer les autres onglets.
    let boot_box = GroupBox::new("Démarrage (systemd-boot)").parent(&boot_page).build();
    let mut boot_l = VBoxLayout::with_parent(&boot_box);
    boot_l.set_spacing(6);
    boot_l.add(
        Label::new(&format!(
            "Noyau actuellement utilisé : {}",
            sysutil::running_kernel()
        ))
        .parent(&boot_box)
        .build(),
    );

    // ---- Jauge « espace libre sur /boot » ---------------------------------
    // La partition de démarrage (ESP) n'est pas montée par défaut sur Solus :
    // `statvfs("/boot")` retomberait alors sur le FS racine. On mesure donc la
    // vraie ESP : sans privilège si elle est déjà montée, sinon via le bouton
    // « Mesurer » (montage lecture seule temporaire, puis démontage).
    let free_bar = Rc::new(RefCell::new(
        ProgressBar::new().range(0, 100).value(0).format("").build(),
    ));
    free_bar.borrow_mut().set_has_parent();
    let free_lbl = Label::new("Espace utilisé sur /boot : —").parent(&boot_box).build();
    let free_lbl_ptr = free_lbl.widget_ptr() as *mut qtrs::ffi::QLabel;

    // Affiche (barre + texte) un espace mesuré. La barre représente l'espace
    // **utilisé** (total − libre) : elle se remplit à mesure que la partition
    // se remplit.
    fn apply_space(
        lbl: *mut qtrs::ffi::QLabel,
        bar: &Rc<RefCell<ProgressBar>>,
        sp: sysutil::BootSpace,
    ) {
        let used = sp.total.saturating_sub(sp.free);
        let pct = if sp.total > 0 {
            ((used as f64 / sp.total as f64) * 100.0).round() as i32
        } else {
            0
        };
        bar.borrow_mut().set_value(pct.clamp(0, 100));
        set_label_text(
            lbl,
            &format!(
                "Espace utilisé sur /boot : {} / {} ({} libre)",
                fmt_bytes(used),
                fmt_bytes(sp.total),
                fmt_bytes(sp.free)
            ),
        );
    }

    match sysutil::boot_space_nopass() {
        Some(sp) => apply_space(free_lbl_ptr, &free_bar, sp),
        None => {
            let msg = match sysutil::esp_size() {
                Some(t) => format!(
                    "Espace utilisé sur /boot : inconnu (partition non montée, {} au total) — cliquez sur « Mesurer »",
                    fmt_bytes(t)
                ),
                None => "Espace utilisé sur /boot : inconnu — cliquez sur « Mesurer »".to_string(),
            };
            set_label_text(free_lbl_ptr, &msg);
        }
    }

    let free_row_w = Widget::new().parent(&boot_box).build();
    let mut free_row_l = HBoxLayout::with_parent(&free_row_w);
    free_row_l.set_spacing(8);
    free_row_l.add(free_lbl);

    // Barre directement dans le layout (plus de ScrollArea qui gonflait le
    // cadre) avec une hauteur fixe : épaissie pour rester lisible.
    let free_sa = wrap_scroll(&free_bar, &boot_page);
    unsafe { qtrs::ffi::QWidget_setFixedHeight(free_sa.widget_ptr(), 34) };
    free_row_l.add(free_sa);

    // Bouton « Mesurer » : demande le mot de passe root, monte l'ESP en
    // lecture seule le temps de la mesure, puis la démonte.
    let measure_btn = PushButton::new("Mesurer")
        .on_clicked({
            let bar = free_bar.clone();
            let logw = logw.clone();
            let status = status.clone();
            move || {
                if let Some(pw) = sysutil::ask_root_password() {
                    set_status(&status, "Mesure de l'espace /boot…");
                    match sysutil::boot_space_with_sudo(&pw) {
                        Ok(sp) => {
                            apply_space(free_lbl_ptr, &bar, sp);
                            append_log(
                                &logw,
                                &format!(
                                    "Espace /boot : {} libre sur {}",
                                    fmt_bytes(sp.free),
                                    fmt_bytes(sp.total)
                                ),
                            );
                            set_status(&status, "Espace /boot mesuré.");
                        }
                        Err(e) => {
                            append_log(&logw, &format!("ERREUR mesure /boot : {}", e));
                            set_status(&status, "Échec de la mesure de /boot.");
                        }
                    }
                }
            }
        })
        .parent(&free_row_w)
        .build();
    free_row_l.add(measure_btn);
    boot_l.add(free_row_w);

    let boot_to_lbl = Label::new("Délai du menu de démarrage : (cliquez sur « Lire »)")
        .parent(&boot_box)
        .build();
    let boot_to_ptr = boot_to_lbl.widget_ptr() as *mut qtrs::ffi::QLabel;
    boot_l.add(boot_to_lbl);

    let boot_row_w = Widget::new().parent(&boot_box).build();
    let mut boot_row_l = HBoxLayout::with_parent(&boot_row_w);
    boot_row_l.set_spacing(8);
    boot_row_l.add(Label::new("Délai du menu (secondes) :").parent(&boot_row_w).build());
    let delay_spin = SpinBox::new()
        .range(0, 60)
        .value(5)
        .parent(&boot_row_w)
        .build();
    let delay_ptr = delay_spin.widget_ptr() as *mut qtrs::ffi::QSpinBox;
    boot_row_l.add(delay_spin);

    let read_btn = PushButton::new("Lire")
        .on_clicked({
            let logw = logw.clone();
            let status = status.clone();
            move || {
                if let Some(pw) = sysutil::ask_root_password() {
                    match sysutil::boot_timeout_get(&pw) {
                        Ok(v) => {
                            set_label_text(
                                boot_to_ptr,
                                &format!("Délai du menu de démarrage : {}", v),
                            );
                            append_log(&logw, &format!("get-timeout → {}", v));
                            set_status(&status, &format!("Délai du menu : {}", v));
                        }
                        Err(e) => {
                            append_log(&logw, &format!("ERREUR get-timeout : {}", e));
                            set_status(&status, "Échec de la lecture du délai.");
                        }
                    }
                }
            }
        })
        .parent(&boot_row_w)
        .build();
    boot_row_l.add(read_btn);

    let set_btn = PushButton::new("Définir")
        .on_clicked({
            let logw = logw.clone();
            let status = status.clone();
            move || {
                if let Some(pw) = sysutil::ask_root_password() {
                    let secs = unsafe { qtrs::ffi::QSpinBox_value(delay_ptr) }.max(0) as u32;
                    match sysutil::boot_timeout_set(&pw, secs) {
                        Ok(_) => {
                            let shown = sysutil::boot_timeout_get(&pw)
                                .unwrap_or_else(|_| format!("{} secondes", secs));
                            set_label_text(
                                boot_to_ptr,
                                &format!("Délai du menu de démarrage : {}", shown),
                            );
                            append_log(&logw, &format!("set-timeout {} → {}", secs, shown));
                            set_status(&status, &format!("Délai du menu : {}", shown));
                        }
                        Err(e) => {
                            append_log(&logw, &format!("ERREUR set-timeout : {}", e));
                            set_status(&status, "Échec de la définition du délai.");
                        }
                    }
                }
            }
        })
        .parent(&boot_row_w)
        .build();
    boot_row_l.add(set_btn);

    boot_l.add(boot_row_w);
    // Le groupe occupe la hauteur de l'onglet : sans ce ressort, ses quatre
    // lignes se répartissent sur toute la hauteur (grands vides).
    boot_l.add_spacer(Spacer::vertical_expanding());
    boot_page_l.add(boot_box);
    tabs.add_tab(Box::new(boot_page), "systemd-boot");
    // Hook de test : ouvre directement un onglet au démarrage, pour cadrer une
    // capture d'écran sans piloter la souris (`SEKM_TAB=3`).
    if let Ok(v) = std::env::var("SEKM_TAB") {
        if let Ok(i) = v.parse::<i32>() {
            tabs.set_current_index(i);
        }
    }
    // `widget_ptr()` renvoie un `*mut QWidget` : lobjet est bien un QTabWidget
    // (adresse identique, héritage simple), comme pour QLabel/QSpinBox ailleurs.
    let tabs_ptr = tabs.widget_ptr() as *mut qtrs::ffi::QTabWidget;
    apply_tab_height(tabs_ptr);
    root.add(tabs);

    // Groupe « installés » (commun)
    let inst_box = GroupBox::new("Noyaux externes installés sur cette machine").parent(wp).build();
    let mut inst_l = VBoxLayout::with_parent(&inst_box);
    inst_l.add(wrap_scroll(&list, wp));
    root.add(inst_box);


    // Barre de progression + statut (communs).
    let bar_row_w = Widget::new().parent(wp).build();
    let mut bar_row_l = HBoxLayout::with_parent(&bar_row_w);
    bar_row_l.set_spacing(10);
    bar_row_l.add(wrap_scroll(&bar, wp));
    bar_row_l.add(wrap_scroll(&status, wp));
    // Accès direct au journal : la zone du bas est souvent hors écran, et les
    // erreurs de compilation y sont désormais recopiées intégralement.
    {
        let logw = logw.clone();
        let jbtn = PushButton::new("Afficher le journal")
            .on_clicked(move || {
                let body = logw.borrow().plain_text();
                if body.trim().is_empty() {
                    sysutil::show_text_window(
                        "Journal des opérations",
                        "(le journal est vide pour le moment)",
                    );
                } else {
                    sysutil::show_text_window("Journal des opérations", &body);
                }
            })
            .parent(&bar_row_w)
            .build();
        bar_row_l.add(jbtn);
    }
    root.add(bar_row_w);

    // Journal (commun).
    root.add(wrap_scroll(&logw, wp));

    // Enveloppe défilante.
    let central_scroll = ScrollArea::new().parent(wp).set_widget_resizable(true).build();
    central_scroll.set_widget(&central);

    drop(w);
    window.borrow().set_central_widget(&central_scroll);

    Handles {
        tables: tables_arr,
        kernel_lists,
        list,
        bar,
        free_bar,
        status,
        logw,
        central,
        central_scroll,
        layouts: Layouts {
            root,
            avail: avail_l_res,
            actions: actions_l_res,
            inst: inst_l,
            boot: boot_l,
            boot_row: boot_row_l,
            bar_row: bar_row_l,
            free_row: free_row_l,
            adv: adv_layouts,
            boot_page: boot_page_l,
        },
        menubar,
        adv: adv_handles,
        tabs: tabs_ptr,
    }
}

// ------------------------------------------------------------------
// Installation CLI (sans GUI)
// ------------------------------------------------------------------
fn run_cli_install(kind: KernelKind, pw: &str) -> i32 {
    use repo::KernelRelease;
    let kernels = match fetch_latest_kernels(kind) {
        Ok(k) => k,
        Err(e) => {
            eprintln!("ERREUR dépôt {} : {}", kind.label(), e);
            return 1;
        }
    };
    let k: &KernelRelease = &kernels[0];
    println!("Installation de {} ({})", k.kernel_version, kind.label());
    let step = |i: u32, t: &str| println!("[{}/8] {}", i, t);
    match download_kernel(k, &|r, tot| {
        if tot > 0 && r % (8 * 1024 * 1024) < 262_144 {
            println!("  téléchargement : {} / {} Mo", r / 1_048_576, tot / 1_048_576);
        }
    }) {
        Ok(path) => match install::install_kernel(pw, &path, &k.kernel_version, k.kind, &step) {
            Ok(entry) => {
                println!(
                    "NOYAU INSTALLÉ ✓\n  vmlinuz : {}\n  modules : {}\n  boot entry : {}",
                    k.kernel_version,
                    crate::sysutil::KernelPaths::for_version(&k.kernel_version)
                        .modules
                        .display(),
                    entry
                );
                0
            }
            Err(e) => {
                eprintln!("ERREUR Installation : {}", e);
                1
            }
        },
        Err(e) => {
            eprintln!("ERREUR Téléchargement : {}", e);
            1
        }
    }
}

// ------------------------------------------------------------------
// main
// ------------------------------------------------------------------
/// Cherche le logo (PNG 128) dans les emplacements plausibles :
///  1. `/usr/share/icons/hicolor/128x128/apps/solus-external-kernel-manager.png` (paqueté)
///  2. à côté du binaire courant (dev / exécution directe)
/// Retourne `None` si aucun (icône manquante non bloquant).
fn find_logo() -> Option<String> {
    let mut candidates: Vec<String> = vec![
        "/usr/share/icons/hicolor/128x128/apps/solus-external-kernel-manager.png".to_string(),
    ];
    if let Ok(exe) = std::env::current_exe() {
        if let Some(dir) = exe.parent() {
            candidates.push(dir.join("logo-128.png").to_string_lossy().into_owned());
        }
    }
    candidates.push("logo-128.png".to_string());
    for c in candidates {
        if std::path::Path::new(&c).exists() {
            return Some(c);
        }
    }
    None
}


/// `BuildUi` côté ligne de commande : journal ligne par ligne, statut affiché
/// seulement quand le pourcentage change (le téléchargement appelle la
/// progression tous les 256 Ko — sans ce filtre la sortie est illisible).
struct CliUi {
    last_pct: std::cell::Cell<i32>,
}

impl CliUi {
    fn new() -> Self {
        Self {
            last_pct: std::cell::Cell::new(-1),
        }
    }
}

impl BuildUi for CliUi {
    fn log(&self, line: &str) {
        println!("  {}", line);
    }
    fn status(&self, pct: i32, text: &str) {
        if pct != self.last_pct.get() {
            self.last_pct.set(pct);
            println!("[{:3}%] {}", pct, text);
        }
    }
}

/// Valeur d'une option `--clé valeur` dans `args`.
fn arg_value(args: &[String], key: &str) -> Option<String> {
    args.iter()
        .position(|a| a == key)
        .and_then(|i| args.get(i + 1))
        .cloned()
}

/// Modes `--kernelorg-*` (sans GUI) : sert aux tests sur une machine distante
/// et au débogage d'un travail de compilation.
///
///   --kernelorg-list                      liste les versions publiées
///   --kernelorg-stage <version>           télécharge, vérifie la signature,
///                                         extrait et prépare le .config
///   --kernelorg-build <version>           tout, jusqu'à l'installation
///                                         (mot de passe via SEKM_SUDO_PASS)
/// Options : --base current|localmod|defconfig, --preempt keep|none|voluntary|full|lazy,
///           --no-march, --jobs N, --localversion X, --build-root DIR
fn run_kernelorg_cli(mode: &str, args: &[String]) -> i32 {
    let pos = match args.iter().position(|a| a == mode) {
        Some(p) => p,
        None => return 2,
    };
    let version = args.get(pos + 1).cloned().unwrap_or_default();
    if mode == "--kernelorg-list" {
        return match source::fetch_releases() {
            Ok(v) => {
                println!("{} versions publiées sur kernel.org :", v.len());
                for r in &v {
                    // `label()` porte les marqueurs utiles (LTS, EOL, RC non signé).
                    println!("  {}", r.label());
                }
                0
            }
            Err(e) => {
                eprintln!("ERREUR : {}", e);
                1
            }
        };
    }

    let mut opts = BuildOptions::default();
    if let Some(b) = arg_value(args, "--base") {
        match ConfigBase::parse(&b) {
            Some(v) => opts.base = v,
            None => {
                eprintln!("--base invalide : {}", b);
                return 2;
            }
        }
    }
    if let Some(p) = arg_value(args, "--preempt") {
        match Preempt::parse(&p) {
            Some(v) => opts.preempt = v,
            None => {
                eprintln!("--preempt invalide : {}", p);
                return 2;
            }
        }
    }
    if args.iter().any(|a| a == "--no-march") {
        opts.march_native = false;
    }
    if let Some(j) = arg_value(args, "--jobs") {
        match j.parse::<u32>() {
            Ok(n) if n >= 1 => opts.jobs = n,
            _ => {
                eprintln!("--jobs invalide : {}", j);
                return 2;
            }
        }
    }
    if let Some(lv) = arg_value(args, "--localversion") {
        opts.localversion = lv;
    }
    if let Some(br) = arg_value(args, "--build-root") {
        opts.build_root = std::path::PathBuf::from(br);
    }

    let known = source::fetch_releases().unwrap_or_default();
    let rel = match source::resolve_release(&known, &version) {
        Ok(r) => r,
        Err(e) => {
            eprintln!("ERREUR version : {}", e);
            return 2;
        }
    };
    println!(
        "== kernel.org {} ({}) — base {} — préemption {} — march=native {} — -j{} ==",
        rel.version,
        rel.moniker,
        opts.base.label(),
        opts.preempt.label(),
        opts.march_native,
        opts.jobs
    );
    let cancel = Arc::new(AtomicBool::new(false));
    let ui = CliUi::new();
    let allow_unsigned = args.iter().any(|a| a == "--unsigned-ok");
    if mode == "--kernelorg-stage" {
        return match build::prepare(&rel, &opts, &ui, &cancel, allow_unsigned) {
            Ok(p) => {
                println!(
                    "PRÊT ✓ sources : {}\n      uname -r prévu : {}\n      préemption : {}",
                    p.src.display(),
                    p.kver,
                    p.report.preempt_active
                );
                0
            }
            Err(e) => {
                eprintln!("ERREUR préparation : {}", e);
                1
            }
        };
    }
    let pw = std::env::var("SEKM_SUDO_PASS").unwrap_or_default();
    if pw.is_empty() {
        eprintln!("SEKM_SUDO_PASS non définie (requise pour modules_install)");
        return 2;
    }
    match build::run_job(&pw, &rel, &opts, &ui, &cancel) {
        Ok((kver, entry)) => {
            println!("NOYAU COMPILÉ ET INSTALLÉ ✓\n  uname -r : {}\n  entrée : {}", kver, entry);
            0
        }
        Err(e) => {
            eprintln!("ERREUR compilation : {}", e);
            1
        }
    }
}

fn main() {
    // Mode test en ligne de commande : vérifie la lecture des trois dépôts
    // (et, avec --download, le téléchargement + SHA256 d'un paquet) sans GUI.
    // Usage : solus-external-kernel-manager --fetch-test [--download]
    let args: Vec<String> = std::env::args().collect();

    // Modes kernel.org (sans GUI) — voir `run_kernelorg_cli`.
    for mode in [
        "--kernelorg-list",
        "--kernelorg-stage",
        "--kernelorg-build",
    ] {
        if args.iter().any(|a| a == mode) {
            std::process::exit(run_kernelorg_cli(mode, &args));
        }
    }

    // Mode installation CLI (sans GUI) : `sekm --install <cachyos|zen|liquorix>`.
    // Le mot de passe root est fourni via l'env `SEKM_SUDO_PASS`.
    // Sert aux scripts et au test sur machine distante.
    if let Some(pos) = args.iter().position(|a| a == "--install") {
        let kind_name = args.get(pos + 1).cloned().unwrap_or_default().to_lowercase();
        let kind = match kind_name.as_str() {
            "cachyos" => KernelKind::Cachyos,
            "zen" => KernelKind::Zen,
            "liquorix" => KernelKind::Liquorix,
            _ => {
                eprintln!("usage: sekm --install <cachyos|zen|liquorix>");
                std::process::exit(2);
            }
        };
        let pw = std::env::var("SEKM_SUDO_PASS").unwrap_or_default();
        if pw.is_empty() {
            eprintln!("SEKM_SUDO_PASS env non définie");
            std::process::exit(2);
        }
        std::process::exit(run_cli_install(kind, &pw));
    }

    // Mode espace /boot (sans GUI) : `sekm --boot-space` — affiche la taille
    // totale et l'espace libre de la partition de démarrage (ESP). Utilise
    // `SEKM_SUDO_PASS` si la partition n'est pas montée (montage ro temporaire).
    if args.iter().any(|a| a == "--boot-space") {
        match sysutil::boot_space_nopass() {
            Some(sp) => {
                println!(
                    "/boot : {} utilisé / {} ({} libre) [monté]",
                    fmt_bytes(sp.total.saturating_sub(sp.free)),
                    fmt_bytes(sp.total),
                    fmt_bytes(sp.free)
                );
            }
            None => {
                let pw = std::env::var("SEKM_SUDO_PASS").unwrap_or_default();
                match sysutil::esp_size() {
                    Some(t) => println!("/boot : {} au total (non monté)", fmt_bytes(t)),
                    None => println!("/boot : ESP introuvable"),
                }
                if pw.is_empty() {
                    println!("(SEKM_SUDO_PASS non définie : mesure précise impossible)");
                    std::process::exit(0);
                }
                match sysutil::boot_space_with_sudo(&pw) {
                    Ok(sp) => println!(
                        "/boot : {} utilisé / {} ({} libre) [mesuré via montage ro]",
                        fmt_bytes(sp.total.saturating_sub(sp.free)),
                        fmt_bytes(sp.total),
                        fmt_bytes(sp.free)
                    ),
                    Err(e) => {
                        eprintln!("ERREUR mesure /boot : {}", e);
                        std::process::exit(1);
                    }
                }
            }
        }
        std::process::exit(0);
    }

    // Mode liste CLI (sans GUI) : `sekm --list` — liste les noyaux externes
    // installés selon le même code que l'UI (registre + repli par nom).
    if args.iter().any(|a| a == "--list") {
        let running = crate::sysutil::running_kernel();
        let inst = list_installed();
        if inst.is_empty() {
            println!("Aucun noyau externe installé.");
        } else {
            println!("Noyaux externes installés (en cours : {}):", running);
            for k in &inst {
                println!(
                    "  [{}] {}  version: {}  display: {}  modules: {}",
                    if k.running { "RUNNING" } else { "..." },
                    k.kind.label(),
                    k.version,
                    k.display_version,
                    k.path.modules.display()
                );
            }
        }
        // Le nom des fichiers ESP correspondants (vérifiable par l'utilisateur).
        println!("\nEntrées de boot liées :");
        for k in &inst {
            println!("  {}", k.path.boot_entry.display());
        }
        std::process::exit(0);
    }

    if args.iter().any(|a| a == "--fetch-test") {
        let do_download = args.iter().any(|a| a == "--download");
        let kinds: [KernelKind; 3] = [
            KernelKind::Cachyos,
            KernelKind::Zen,
            KernelKind::Liquorix,
        ];
        let mut overall_ok = true;
        for kind in kinds {
            println!("\n=== Dépôt {} ===", kind.label());
            match fetch_latest_kernels(kind) {
                Ok(kernels) => {
                    println!("{} noyaux trouvés", kernels.len());
                    for k in &kernels {
                        println!(
                            "  {}  (noyau {})  {} Mo  sha256={}",
                            k.version,
                            k.kernel_version,
                            (k.csize as f64 / 1_048_576.0).round(),
                            &k.sha256[..16.min(k.sha256.len())]
                        );
                    }
                    if do_download {
                        let k = &kernels[0];
                        println!("\n=== Téléchargement de {} ({}) … ===", k.version, kind.label());
                        match download_kernel(k, &|recv, tot| {
                            if tot > 0 && recv % (4 * 1024 * 1024) < 262_144 {
                                println!("  {} / {} Mo", recv / 1_048_576, tot / 1_048_576);
                            }
                        }) {
                            Ok(p) => println!("OK : {} (SHA256 vérifié) ✓", p.display()),
                            Err(e) => {
                                eprintln!("ERREUR téléchargement : {}", e);
                                overall_ok = false;
                            }
                        }
                    }
                }
                Err(e) => {
                    eprintln!("ERREUR dépôt {} : {}", kind.label(), e);
                    overall_ok = false;
                }
            }
        }
        std::process::exit(if overall_ok { 0 } else { 1 });
    }

    let app = Application::new();
    // Icône de fenêtre (tux + clé à molette) — chargée du PNG, si trouvé.
    if let Some(p) = find_logo() {
        app.set_icon(&p);
    }
    let (tx, rx) = mpsc::channel::<WorkerMsg>();

    let window = Rc::new(RefCell::new(
        MainWindow::new()
            .window_title("Solus External Kernel Manager")
            .size(1050, 760)
            .build(),
    ));

    let tx_hook = tx.clone();
    let h = build_ui(&window, tx);
    refresh_installed_list(&h.list);
    window.borrow().show();

    // ---- Hooks de test headless -------------------------------------------
    // SEKM_AUTOFETCH=1 : récupère la liste kernel.org au démarrage (capture).
    if std::env::var("SEKM_AUTOFETCH").is_ok() {
        spawn_org_fetch(&tx_hook, &h.logw);
    }
    // SEKM_SMOKE_MS=1500 : quitte avec 0 après N ms. Un code 139 (SIGSEGV)
    // signale un arbre de widgets cassé ; 0 prouve seulement que la boucle
    // d'événements a démarré (la capture d'écran reste la preuve du rendu).
    if let Some(ms) = std::env::var("SEKM_SMOKE_MS")
        .ok()
        .and_then(|v| v.parse::<i32>().ok())
    {
        Timer::single_shot(ms, || std::process::exit(0));
    }

    // ---- Timer « pompe » : file worker → widgets ---------------------------
    let _timer = Timer::new(400)
        .on_timeout({
            let window = Rc::clone(&window);
            let tables = [
                Rc::clone(&h.tables[0]),
                Rc::clone(&h.tables[1]),
                Rc::clone(&h.tables[2]),
            ];
            let kernel_lists: [Rc<RefCell<Vec<KernelRelease>>>; 3] = [
                Rc::clone(&h.kernel_lists[0]),
                Rc::clone(&h.kernel_lists[1]),
                Rc::clone(&h.kernel_lists[2]),
            ];
            let list = Rc::clone(&h.list);
            let bar = Rc::clone(&h.bar);
            let status = Rc::clone(&h.status);
            let logw = Rc::clone(&h.logw);
            let adv_combo = h.adv.combo;
            let adv_status = h.adv.status;
            let adv_versions = Rc::clone(&h.adv.versions);
            let adv_prepared = Rc::clone(&h.adv.prepared);
            let adv_base = h.adv.base;
            let adv_base_note = h.adv.base_note;
            let last_base_note = std::cell::RefCell::new(String::new());
            let tabs_ptr = h.tabs;
            let last_tab = std::cell::Cell::new(unsafe {
                qtrs::ffi::QTabWidget_currentIndex(tabs_ptr)
            });
            let rx: mpsc::Receiver<WorkerMsg> = rx;
            move || {
                // La hauteur du TabWidget suit l'onglet affiché (voir
                // `apply_tab_height`) : Qt fige sinon celle du plus grand.
                // Note de la base : une seule explication visible, celle de la
                // base réellement sélectionnée (lu par sondage, pas par signal :
                // aucun gestionnaire à faire survivre au layout).
                let sel = unsafe { qtrs::ffi::QComboBox_currentText(adv_base) };
                if *last_base_note.borrow() != sel {
                    *last_base_note.borrow_mut() = sel.clone();
                    set_label_text(adv_base_note, base_note_for(&sel));
                }
                let cur = unsafe { qtrs::ffi::QTabWidget_currentIndex(tabs_ptr) };
                if cur != last_tab.get() {
                    last_tab.set(cur);
                    apply_tab_height(tabs_ptr);
                }
                let mut done: Option<(bool, String)> = None;
                while let Ok(msg) = rx.try_recv() {
                    match msg {
                        WorkerMsg::Progress { pct, text } => {
                            bar.borrow_mut().set_value(pct);
                            set_status(&status, &text);
                        }
                        WorkerMsg::Log(line) => append_log(&logw, &line),
                        WorkerMsg::Done { ok, message } => done = Some((ok, message)),
                        WorkerMsg::Prepared(p) => {
                            append_log(
                                &logw,
                                &format!("Sources prêtes : {} ({})", p.kver, p.src.display()),
                            );
                            set_label_text(
                                adv_status,
                                &format!(
                                    "Prêt à compiler {} — « 2. Ouvrir menuconfig » ou « 3. Compiler et installer ».",
                                    p.kver
                                ),
                            );
                            *adv_prepared.borrow_mut() = Some(*p);
                        }
                        WorkerMsg::OrgReleases(rel) => {
                            // Remplissage de la liste déroulante de l'onglet
                            // « Avancé » (le libellé sert de clé de sélection).
                            unsafe { qtrs::ffi::QComboBox_clear(adv_combo) };
                            for r in &rel {
                                cxx::let_cxx_string!(c = r.label());
                                unsafe { qtrs::ffi::QComboBox_addItem(adv_combo, &c) };
                            }
                            // Défaut : première version non-rc (la liste
                            // commence par mainline, qui est presque toujours
                            // une rc).
                            let default_idx = rel
                                .iter()
                                .position(|r| !r.version.contains("-rc"))
                                .unwrap_or(0) as i32;
                            unsafe { qtrs::ffi::QComboBox_setCurrentIndex(adv_combo, default_idx) };
                            let n = rel.len();
                            *adv_versions.borrow_mut() = rel;
                            append_log(&logw, &format!("kernel.org : {} versions listées.", n));
                            set_label_text(
                                adv_status,
                                &format!(
                                    "{} versions listées — sélectionnez la version puis « 1. Préparer ».",
                                    n
                                ),
                            );
                        }
                        WorkerMsg::Fetch {
                            tag,
                            kernels: fetched,
                            installed,
                        } => {
                            let idx = tag as usize;
                            if idx >= 3 {
                                continue;
                            }
                            let t = &tables[idx];
                            let kl = &kernel_lists[idx];
                            let n = fetched.len().max(1) as i32;
                            let t_guard = t.borrow();
                            t_guard.set_row_count(n);
                            for (i, k) in fetched.iter().enumerate() {
                                let size = format!("{:.0}", k.csize as f64 / 1_048_576.0);
                                let is_installed = installed
                                    .iter()
                                    .any(|x| x.version == k.kernel_version
                                        || x.display_version == k.kernel_version);
                                t_guard.set_item(i as i32, 0, &k.version);
                                t_guard.set_item(i as i32, 1, &k.kernel_version);
                                t_guard.set_item(i as i32, 2, &k.sha256);
                                t_guard.set_item(i as i32, 3, &size);
                                t_guard.set_item(
                                    i as i32,
                                    4,
                                    if is_installed { "installée" } else { "nouvelle ★" },
                                );
                            }
                            drop(t_guard);
                            kl.borrow_mut().clone_from(&fetched);
                            refresh_installed_list(&list);
                            let label = fetched
                                .first()
                                .map(|k| k.kind.label())
                                .unwrap_or("");
                            append_log(&logw, &format!("Dépôt {} interrogé, liste à jour.", label));
                            set_status(
                                &status,
                                "Liste à jour — sélectionnez une version puis « Installer ».",
                            );
                            let _ = &window;
                        }
                    }
                }
                if let Some((ok, message)) = done {
                    // Fin d'opération : la barre quitte le mode « occupé »
                    // (plage 0..0 animée) et revient à un pourcentage.
                    bar.borrow_mut().set_range(0, 100);
                    bar.borrow_mut().set_value(0);
                    // QMessageBox::Icon : 0=Critical, 2=Information ; Ok=0x400.
                    let mb = qtrs::MessageBox::new()
                        .icon(if ok { 2 } else { 0 })
                        .text(if ok { "Terminé" } else { "Échec de l'opération" })
                        .informative_text(&message)
                        .window_title("Solus External Kernel Manager")
                        .standard_buttons(0x400)
                        .build();
                    let _ = mb.exec();
                    if ok {
                        refresh_installed_list(&list);
                        bar.borrow_mut().set_value(0);
                        set_status(&status, "Prêt.");
                    }
                }
            }
        })
        .build();

    app.exec();
}

// ------------------------------------------------------------------
// Tests
// ------------------------------------------------------------------
#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn fmt_bytes_below_kilo() {
        assert_eq!(fmt_bytes(0), "0 o");
        assert_eq!(fmt_bytes(123), "123 o");
        assert_eq!(fmt_bytes(1023), "1023 o");
    }

    #[test]
    fn fmt_bytes_kilo() {
        assert_eq!(fmt_bytes(1024), "1.0 Ko");
        assert_eq!(fmt_bytes(1_536), "1.5 Ko");
        assert_eq!(fmt_bytes(1023 * 1024), "1023.0 Ko");
    }

    #[test]
    fn fmt_bytes_mega_giga() {
        assert_eq!(fmt_bytes(1024 * 1024), "1.0 Mo");
        assert_eq!(fmt_bytes(1_048_576 * 50), "50.0 Mo");
        assert_eq!(fmt_bytes(1024_u64.pow(3)), "1.0 Go");
        assert_eq!(fmt_bytes(1024_u64.pow(3) * 2), "2.0 Go");
    }

    #[test]
    fn fmt_bytes_terabyte_caps() {
        assert_eq!(fmt_bytes(1024_u64.pow(4)), "1.0 To");
        // Au-delà, on reste en To (pas d'unité supérieure).
        assert_eq!(fmt_bytes(1024_u64.pow(4) * 5), "5.0 To");
    }
}
