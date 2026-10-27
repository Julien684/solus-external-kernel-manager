//! Solus External Kernel Manager — GUI Qt6 (via `qtrs`) : installer/supprimer
//! les noyaux externes (CachyOS, Zen, Liquorix) sur Solus.
//!
//! Trois onglets en haut, un par noyau (CachyOS / Zen / Liquorix). Chaque
//! onglet contient sa table de versions disponibles **et ses boutons**
//! (Actualiser / Installer / Supprimer). La zone commune en bas rassemble la
//! liste des « noyaux installés », le démarrage systemd-boot, la barre de
//! progression et le journal.
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

mod install;
mod repo;
mod sysutil;

use install::{list_installed, remove_kernel, InstalledKernel};
use repo::{download_kernel, fetch_latest_kernels, KernelKind, KernelRelease};

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

/// Référence aux widgets partagés (utilisée par les closures boutons).
struct SharedUi {
    tx: mpsc::Sender<WorkerMsg>,
    status: Rc<RefCell<Label>>,
    logw: Rc<RefCell<PlainTextEdit>>,
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
    l.add(wrap_scroll(table, wp));
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
    (page, l, actions_l)
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
    };

    // ---- Menu ---------------------------------------------------------------
    let file_menu = Menu::new("Fichier")
        .action("Quitter", || std::process::exit(0))
        .parent(wp)
        .build();
    let help_menu = Menu::new("?")
        .action("À propos", || {
            qtrs::MessageBox::about(
                None,
                "À propos — Solus External Kernel Manager",
                "Gestionnaire des noyaux externes (CachyOS, Zen, Liquorix) pour Solus.\n\n\
                 • Trois onglets, un par famille de noyaux\n\
                 • Détecte les nouvelles versions dans chaque dépôt pacman\n\
                 • Les installe (vmlinuz, initrd dracut, entrée systemd-boot)\n\
                 • Supprime les noyaux anciens (jamais le noyau en cours)\n\n\
                 /boot est monté via `clr-boot-manager mount-boot`.",
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
    root.add(tabs);

    // Groupe « installés » (commun)
    let inst_box = GroupBox::new("Noyaux externes installés sur cette machine").parent(wp).build();
    let mut inst_l = VBoxLayout::with_parent(&inst_box);
    inst_l.add(wrap_scroll(&list, wp));
    root.add(inst_box);

    // Groupe « démarrage systemd-boot » (commun)
    let boot_box = GroupBox::new("Démarrage (systemd-boot)").parent(wp).build();
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
    root.add(boot_box);

    // Barre de progression + statut (communs).
    let bar_row_w = Widget::new().parent(wp).build();
    let mut bar_row_l = HBoxLayout::with_parent(&bar_row_w);
    bar_row_l.set_spacing(10);
    bar_row_l.add(wrap_scroll(&bar, wp));
    bar_row_l.add(wrap_scroll(&status, wp));
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
        },
        menubar,
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
fn main() {
    // Mode test en ligne de commande : vérifie la lecture des trois dépôts
    // (et, avec --download, le téléchargement + SHA256 d'un paquet) sans GUI.
    // Usage : solus-external-kernel-manager --fetch-test [--download]
    let args: Vec<String> = std::env::args().collect();

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
    let (tx, rx) = mpsc::channel::<WorkerMsg>();

    let window = Rc::new(RefCell::new(
        MainWindow::new()
            .window_title("Solus External Kernel Manager")
            .size(1050, 760)
            .build(),
    ));

    let h = build_ui(&window, tx);
    refresh_installed_list(&h.list);
    window.borrow().show();

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
            let rx: mpsc::Receiver<WorkerMsg> = rx;
            move || {
                let mut done: Option<(bool, String)> = None;
                while let Ok(msg) = rx.try_recv() {
                    match msg {
                        WorkerMsg::Progress { pct, text } => {
                            bar.borrow_mut().set_value(pct);
                            set_status(&status, &text);
                        }
                        WorkerMsg::Log(line) => append_log(&logw, &line),
                        WorkerMsg::Done { ok, message } => done = Some((ok, message)),
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
