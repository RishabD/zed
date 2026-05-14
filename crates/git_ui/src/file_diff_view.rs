//! FileDiffView provides a UI for displaying differences between two buffers.

use anyhow::{Context as _, Result};
use buffer_diff::BufferDiff;
use editor::{Editor, EditorEvent, EditorSettings, MultiBuffer, SplittableEditor};
use futures::{FutureExt, select_biased};
use gpui::{
    AnyElement, App, AppContext as _, AsyncApp, Context, Entity, EventEmitter, FocusHandle,
    Focusable, Font, IntoElement, Render, Task, WeakEntity, Window,
};
use language::{Buffer, HighlightedText, LanguageRegistry};
use project::{Project, ProjectPath};
use settings::Settings;
use std::{
    any::{Any, TypeId},
    path::PathBuf,
    pin::pin,
    sync::Arc,
    time::Duration,
};
use ui::{Color, Icon, IconName, Label, LabelCommon as _, SharedString};
use util::paths::PathExt as _;
use workspace::{
    Item, ItemHandle as _, ItemNavHistory, ToolbarItemLocation, Workspace,
    item::{ItemEvent, SaveOptions, TabContentParams},
    searchable::SearchableItemHandle,
};

pub struct FileDiffView {
    editor: Entity<SplittableEditor>,
    old_buffer: Option<Entity<Buffer>>,
    new_buffer: Entity<Buffer>,
    title: SharedString,
    tooltip: Option<SharedString>,
    buffer_changes_tx: watch::Sender<()>,
    _recalculate_diff_task: Task<Result<()>>,
}

const RECALCULATE_DIFF_DEBOUNCE: Duration = Duration::from_millis(250);

impl FileDiffView {
    #[ztracing::instrument(skip_all)]
    pub fn open(
        old_path: PathBuf,
        new_path: PathBuf,
        workspace: WeakEntity<Workspace>,
        window: &mut Window,
        cx: &mut App,
    ) -> Task<Result<Entity<Self>>> {
        window.spawn(cx, async move |cx| {
            let project = workspace.update(cx, |workspace, _| workspace.project().clone())?;
            let old_buffer = project
                .update(cx, |project, cx| project.open_local_buffer(&old_path, cx))
                .await?;
            let new_buffer = project
                .update(cx, |project, cx| project.open_local_buffer(&new_path, cx))
                .await?;
            let languages = project.update(cx, |project, _| project.languages().clone());

            let buffer_diff = build_buffer_diff(&old_buffer, &new_buffer, languages, cx).await?;

            workspace.update_in(cx, |workspace, window, cx| {
                let workspace_entity = cx.entity();
                let diff_view = cx.new(|cx| {
                    let title = title_for_buffers(&old_buffer, &new_buffer, cx);
                    let tooltip = tooltip_for_buffers(&old_buffer, &new_buffer, cx);
                    FileDiffView::new(
                        Some(old_buffer),
                        new_buffer,
                        buffer_diff,
                        project.clone(),
                        workspace_entity,
                        title,
                        tooltip,
                        true,
                        window,
                        cx,
                    )
                });

                let pane = workspace.active_pane();
                pane.update(cx, |pane, cx| {
                    pane.add_item(Box::new(diff_view.clone()), true, true, None, window, cx);
                });

                diff_view
            })
        })
    }

    pub fn open_existing_diff(
        new_buffer: Entity<Buffer>,
        diff: Entity<BufferDiff>,
        base_label: SharedString,
        project: Entity<Project>,
        workspace: WeakEntity<Workspace>,
        split: bool,
        window: &mut Window,
        cx: &mut App,
    ) -> Task<Result<Entity<Self>>> {
        window.spawn(cx, async move |cx| {
            workspace.update_in(cx, |workspace, window, cx| {
                let workspace_entity = cx.entity();
                let filename = filename_for_buffer(&new_buffer, cx);
                let path = path_for_buffer(&new_buffer, cx);
                let title = format!("{} ↔ {}", base_label, filename).into();
                let tooltip = Some(format!("{} ↔ {}", base_label, path).into());
                let diff_view = cx.new(|cx| {
                    FileDiffView::new(
                        None,
                        new_buffer,
                        diff,
                        project,
                        workspace_entity,
                        title,
                        tooltip,
                        false,
                        window,
                        cx,
                    )
                });

                let pane = if split {
                    workspace.adjacent_pane(window, cx)
                } else {
                    workspace.active_pane().clone()
                };
                pane.update(cx, |pane, cx| {
                    pane.add_item(Box::new(diff_view.clone()), true, true, None, window, cx);
                });

                diff_view
            })
        })
    }

    pub fn new(
        old_buffer: Option<Entity<Buffer>>,
        new_buffer: Entity<Buffer>,
        diff: Entity<BufferDiff>,
        project: Entity<Project>,
        workspace: Entity<Workspace>,
        title: SharedString,
        tooltip: Option<SharedString>,
        recalculate_diff_on_buffer_change: bool,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Self {
        let multibuffer = cx.new(|cx| {
            let mut multibuffer = MultiBuffer::singleton(new_buffer.clone(), cx);
            multibuffer.add_diff(diff.clone(), cx);
            multibuffer
        });
        let editor = cx.new(|cx| {
            let splittable = SplittableEditor::new(
                EditorSettings::get_global(cx).diff_view_style,
                multibuffer.clone(),
                project.clone(),
                workspace,
                window,
                cx,
            );
            splittable.rhs_editor().update(cx, |editor, _| {
                editor.start_temporary_diff_override();
            });
            splittable.disable_diff_hunk_controls(cx);
            splittable
        });

        let (buffer_changes_tx, mut buffer_changes_rx) = watch::channel(());

        if recalculate_diff_on_buffer_change {
            if let Some(old_buffer) = &old_buffer {
                for buffer in [old_buffer, &new_buffer] {
                    cx.subscribe(buffer, move |this, _, event, _| match event {
                        language::BufferEvent::Edited { .. }
                        | language::BufferEvent::LanguageChanged(_)
                        | language::BufferEvent::Reparsed => {
                            this.buffer_changes_tx.send(()).ok();
                        }
                        _ => {}
                    })
                    .detach();
                }
            }
        }

        let recalculate_diff_task = if recalculate_diff_on_buffer_change {
            cx.spawn(async move |this, cx| {
                while buffer_changes_rx.recv().await.is_ok() {
                    loop {
                        let mut timer = cx
                            .background_executor()
                            .timer(RECALCULATE_DIFF_DEBOUNCE)
                            .fuse();
                        let mut recv = pin!(buffer_changes_rx.recv().fuse());
                        select_biased! {
                            _ = timer => break,
                            _ = recv => continue,
                        }
                    }

                    log::trace!("start recalculating");
                    let (old_snapshot, new_snapshot) = this.update(cx, |this, cx| {
                        let old_buffer = this
                            .old_buffer
                            .as_ref()
                            .context("missing old buffer for recalculating file diff")?;
                        Ok::<_, anyhow::Error>((
                            old_buffer.read(cx).snapshot(),
                            this.new_buffer.read(cx).snapshot(),
                        ))
                    })??;
                    diff.update(cx, |diff, cx| {
                        diff.set_base_text(
                            Some(old_snapshot.text().as_str().into()),
                            old_snapshot.language().cloned(),
                            new_snapshot.text.clone(),
                            cx,
                        )
                    })
                    .await
                    .ok();
                    log::trace!("finish recalculating");
                }
                Ok(())
            })
        } else {
            Task::ready(Ok(()))
        };

        Self {
            editor,
            buffer_changes_tx,
            old_buffer,
            new_buffer,
            title,
            tooltip,
            _recalculate_diff_task: recalculate_diff_task,
        }
    }
}

fn filename_for_buffer(buffer: &Entity<Buffer>, cx: &App) -> String {
    buffer
        .read(cx)
        .file()
        .and_then(|file| file.full_path(cx).file_name()?.to_str().map(str::to_owned))
        .unwrap_or_else(|| "untitled".into())
}

fn path_for_buffer(buffer: &Entity<Buffer>, cx: &App) -> String {
    buffer
        .read(cx)
        .file()
        .map(|file| file.full_path(cx).compact().to_string_lossy().into_owned())
        .unwrap_or_else(|| "untitled".into())
}

fn title_for_buffers(
    old_buffer: &Entity<Buffer>,
    new_buffer: &Entity<Buffer>,
    cx: &App,
) -> SharedString {
    format!(
        "{} ↔ {}",
        filename_for_buffer(old_buffer, cx),
        filename_for_buffer(new_buffer, cx)
    )
    .into()
}

fn tooltip_for_buffers(
    old_buffer: &Entity<Buffer>,
    new_buffer: &Entity<Buffer>,
    cx: &App,
) -> Option<SharedString> {
    Some(
        format!(
            "{} ↔ {}",
            path_for_buffer(old_buffer, cx),
            path_for_buffer(new_buffer, cx)
        )
        .into(),
    )
}

#[ztracing::instrument(skip_all)]
async fn build_buffer_diff(
    old_buffer: &Entity<Buffer>,
    new_buffer: &Entity<Buffer>,
    language_registry: Arc<LanguageRegistry>,
    cx: &mut AsyncApp,
) -> Result<Entity<BufferDiff>> {
    let old_buffer_snapshot = old_buffer.read_with(cx, |buffer, _| buffer.snapshot());
    let new_buffer_snapshot = new_buffer.read_with(cx, |buffer, _| buffer.snapshot());

    let diff = cx.new(|cx| BufferDiff::new(&new_buffer_snapshot.text, cx));

    let update = diff
        .update(cx, |diff, cx| {
            diff.update_diff(
                new_buffer_snapshot.text.clone(),
                Some(old_buffer_snapshot.text().into()),
                Some(true),
                new_buffer_snapshot.language().cloned(),
                cx,
            )
        })
        .await;

    diff.update(cx, |diff, cx| {
        diff.language_changed(
            new_buffer_snapshot.language().cloned(),
            Some(language_registry),
            cx,
        );
        diff.set_snapshot(update, &new_buffer_snapshot.text, cx)
    })
    .await;

    Ok(diff)
}

impl EventEmitter<EditorEvent> for FileDiffView {}

impl Focusable for FileDiffView {
    fn focus_handle(&self, cx: &App) -> FocusHandle {
        self.editor.focus_handle(cx)
    }
}

impl Item for FileDiffView {
    type Event = EditorEvent;

    fn tab_icon(&self, _window: &Window, _cx: &App) -> Option<Icon> {
        Some(Icon::new(IconName::Diff).color(Color::Muted))
    }

    fn tab_content(&self, params: TabContentParams, _window: &Window, cx: &App) -> AnyElement {
        Label::new(self.tab_content_text(params.detail.unwrap_or_default(), cx))
            .color(if params.selected {
                Color::Default
            } else {
                Color::Muted
            })
            .into_any_element()
    }

    fn tab_content_text(&self, _detail: usize, _cx: &App) -> SharedString {
        self.title.clone()
    }

    fn tab_tooltip_text(&self, _cx: &App) -> Option<ui::SharedString> {
        self.tooltip.clone()
    }

    fn to_item_events(event: &EditorEvent, f: &mut dyn FnMut(ItemEvent)) {
        Editor::to_item_events(event, f)
    }

    fn telemetry_event_text(&self) -> Option<&'static str> {
        Some("Diff View Opened")
    }

    fn deactivated(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        self.editor.deactivated(window, cx);
    }

    fn act_as_type<'a>(
        &'a self,
        type_id: TypeId,
        self_handle: &'a Entity<Self>,
        cx: &'a App,
    ) -> Option<gpui::AnyEntity> {
        if type_id == TypeId::of::<Self>() {
            Some(self_handle.clone().into())
        } else {
            self.editor.act_as_type(type_id, cx)
        }
    }

    fn as_searchable(&self, _: &Entity<Self>, _: &App) -> Option<Box<dyn SearchableItemHandle>> {
        Some(Box::new(self.editor.clone()))
    }

    fn for_each_project_item(
        &self,
        cx: &App,
        f: &mut dyn FnMut(gpui::EntityId, &dyn project::ProjectItem),
    ) {
        self.editor.for_each_project_item(cx, f)
    }

    fn active_project_path(&self, cx: &App) -> Option<ProjectPath> {
        self.editor.read(cx).active_project_path(cx)
    }

    fn set_nav_history(
        &mut self,
        nav_history: ItemNavHistory,
        _: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.editor.update(cx, |editor, cx| {
            editor.rhs_editor().update(cx, |editor, _| {
                editor.set_nav_history(Some(nav_history));
            })
        });
    }

    fn navigate(
        &mut self,
        data: Arc<dyn Any + Send>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> bool {
        self.editor.update(cx, |editor, cx| {
            editor
                .rhs_editor()
                .update(cx, |editor, cx| editor.navigate(data, window, cx))
        })
    }

    fn breadcrumb_location(&self, _: &App) -> ToolbarItemLocation {
        ToolbarItemLocation::PrimaryLeft
    }

    fn breadcrumbs(&self, cx: &App) -> Option<(Vec<HighlightedText>, Option<Font>)> {
        self.editor.breadcrumbs(cx)
    }

    fn added_to_workspace(
        &mut self,
        workspace: &mut Workspace,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.editor.update(cx, |editor, cx| {
            editor.rhs_editor().update(cx, |editor, cx| {
                editor.added_to_workspace(workspace, window, cx)
            })
        });
    }

    fn can_save(&self, cx: &App) -> bool {
        self.editor.read(cx).rhs_editor().read(cx).can_save(cx)
    }

    fn save(
        &mut self,
        options: SaveOptions,
        project: Entity<Project>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Task<Result<()>> {
        self.editor.save(options, project, window, cx)
    }
}

impl Render for FileDiffView {
    fn render(&mut self, _: &mut Window, _: &mut Context<Self>) -> impl IntoElement {
        self.editor.clone()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use editor::test::editor_test_context::assert_state_with_diff;
    use gpui::BorrowAppContext;
    use gpui::TestAppContext;
    use project::{FakeFs, Fs, Project};
    use settings::{DiffViewStyle, SettingsStore};
    use std::path::PathBuf;
    use unindent::unindent;
    use util::path;
    use workspace::MultiWorkspace;

    fn init_test(cx: &mut TestAppContext) {
        cx.update(|cx| {
            let settings_store = SettingsStore::test(cx);
            cx.set_global(settings_store);
            cx.update_global::<SettingsStore, _>(|store, cx| {
                store.update_user_settings(cx, |settings| {
                    settings.editor.diff_view_style = Some(DiffViewStyle::Unified);
                });
            });
            theme_settings::init(theme::LoadThemes::JustBase, cx);
        });
    }

    #[gpui::test]
    async fn test_diff_view(cx: &mut TestAppContext) {
        init_test(cx);

        let fs = FakeFs::new(cx.executor());
        fs.insert_tree(
            path!("/test"),
            serde_json::json!({
                "old_file.txt": "old line 1\nline 2\nold line 3\nline 4\n",
                "new_file.txt": "new line 1\nline 2\nnew line 3\nline 4\n"
            }),
        )
        .await;

        let project = Project::test(fs.clone(), [path!("/test").as_ref()], cx).await;

        let (multi_workspace, cx) =
            cx.add_window_view(|window, cx| MultiWorkspace::test_new(project.clone(), window, cx));
        let workspace = multi_workspace.read_with(cx, |mw, _| mw.workspace().clone());

        let diff_view = workspace
            .update_in(cx, |workspace, window, cx| {
                FileDiffView::open(
                    path!("/test/old_file.txt").into(),
                    path!("/test/new_file.txt").into(),
                    workspace.weak_handle(),
                    window,
                    cx,
                )
            })
            .await
            .unwrap();

        // Verify initial diff
        assert_state_with_diff(
            &diff_view.read_with(cx, |diff_view, cx| {
                diff_view.editor.read(cx).rhs_editor().clone()
            }),
            cx,
            &unindent(
                "
                - old line 1
                + ˇnew line 1
                  line 2
                - old line 3
                + new line 3
                  line 4
                ",
            ),
        );

        // Modify the new file on disk
        fs.save(
            path!("/test/new_file.txt").as_ref(),
            &unindent(
                "
                new line 1
                line 2
                new line 3
                line 4
                new line 5
                ",
            )
            .into(),
            Default::default(),
        )
        .await
        .unwrap();

        // The diff now reflects the changes to the new file
        cx.executor().advance_clock(RECALCULATE_DIFF_DEBOUNCE);
        assert_state_with_diff(
            &diff_view.read_with(cx, |diff_view, cx| {
                diff_view.editor.read(cx).rhs_editor().clone()
            }),
            cx,
            &unindent(
                "
                - old line 1
                + ˇnew line 1
                  line 2
                - old line 3
                + new line 3
                  line 4
                + new line 5
                ",
            ),
        );

        // Modify the old file on disk
        fs.save(
            path!("/test/old_file.txt").as_ref(),
            &unindent(
                "
                new line 1
                line 2
                old line 3
                line 4
                ",
            )
            .into(),
            Default::default(),
        )
        .await
        .unwrap();

        // The diff now reflects the changes to the new file
        cx.executor().advance_clock(RECALCULATE_DIFF_DEBOUNCE);
        assert_state_with_diff(
            &diff_view.read_with(cx, |diff_view, cx| {
                diff_view.editor.read(cx).rhs_editor().clone()
            }),
            cx,
            &unindent(
                "
                  ˇnew line 1
                  line 2
                - old line 3
                + new line 3
                  line 4
                + new line 5
                ",
            ),
        );

        diff_view.read_with(cx, |diff_view, cx| {
            assert_eq!(
                diff_view.tab_content_text(0, cx),
                "old_file.txt ↔ new_file.txt"
            );
            assert_eq!(
                diff_view.tab_tooltip_text(cx).unwrap(),
                format!(
                    "{} ↔ {}",
                    path!("test/old_file.txt"),
                    path!("test/new_file.txt")
                )
            );
        })
    }

    #[gpui::test]
    async fn test_save_changes_in_diff_view(cx: &mut TestAppContext) {
        init_test(cx);

        let fs = FakeFs::new(cx.executor());
        fs.insert_tree(
            path!("/test"),
            serde_json::json!({
                "old_file.txt": "old line 1\nline 2\nold line 3\nline 4\n",
                "new_file.txt": "new line 1\nline 2\nnew line 3\nline 4\n"
            }),
        )
        .await;

        let project = Project::test(fs.clone(), ["/test".as_ref()], cx).await;

        let (multi_workspace, cx) =
            cx.add_window_view(|window, cx| MultiWorkspace::test_new(project.clone(), window, cx));
        let workspace = multi_workspace.read_with(cx, |mw, _| mw.workspace().clone());

        let diff_view = workspace
            .update_in(cx, |workspace, window, cx| {
                FileDiffView::open(
                    PathBuf::from(path!("/test/old_file.txt")),
                    PathBuf::from(path!("/test/new_file.txt")),
                    workspace.weak_handle(),
                    window,
                    cx,
                )
            })
            .await
            .unwrap();

        diff_view.update_in(cx, |diff_view, window, cx| {
            diff_view.editor.update(cx, |splittable, cx| {
                splittable.rhs_editor().update(cx, |editor, cx| {
                    editor.insert("modified ", window, cx);
                });
            });
        });

        diff_view.update_in(cx, |diff_view, _, cx| {
            let buffer = diff_view.new_buffer.read(cx);
            assert!(buffer.is_dirty(), "Buffer should be dirty after edits");
        });

        let save_task = diff_view.update_in(cx, |diff_view, window, cx| {
            workspace::Item::save(
                diff_view,
                workspace::item::SaveOptions::default(),
                project.clone(),
                window,
                cx,
            )
        });

        save_task.await.expect("Save should succeed");

        let saved_content = fs.load(path!("/test/new_file.txt").as_ref()).await.unwrap();
        assert_eq!(
            saved_content,
            "modified new line 1\nline 2\nnew line 3\nline 4\n"
        );

        diff_view.update_in(cx, |diff_view, _, cx| {
            let buffer = diff_view.new_buffer.read(cx);
            assert!(!buffer.is_dirty(), "Buffer should not be dirty after save");
        });
    }
}
