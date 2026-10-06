//! Starting the `claude` CLI from Zed.
//!
//! The title bar button opens a terminal in a pane beside the one the user is
//! working in and types `claude` into it. Started from Zed's terminal, the CLI
//! connects to the IDE server on its own.

use crate::{ClaudeCode, dedicated_pane};
use anyhow::{Result, anyhow};
use futures::FutureExt as _;
use gpui::{App, AppContext as _, Context, Entity, Task, Window};
use std::time::Duration;
use terminal::Terminal;
use terminal_view::{TerminalView, default_working_directory};
use workspace::{Workspace, notifications::NotifyTaskExt as _};
use zed_actions::claude_code::NewTerminal;

const CLI_COMMAND: &str = "claude";

/// How long the shell gets to finish starting up before the command is typed
/// regardless.
const SHELL_STARTUP_TIMEOUT: Duration = Duration::from_secs(5);

pub(crate) fn observe_workspaces(cx: &mut Context<ClaudeCode>) {
    cx.observe_new(|workspace: &mut Workspace, _, _| {
        workspace.register_action(|_, _: &NewTerminal, window, cx| {
            let workspace = cx.entity();
            // Splitting off a pane updates the workspace, which is still being
            // updated while this handler runs.
            window.defer(cx, move |window, cx| {
                let Some(claude_code) = ClaudeCode::global(cx) else {
                    return;
                };
                claude_code
                    .update(cx, |this, cx| {
                        this.new_terminal(workspace.clone(), window, cx)
                    })
                    .detach_and_notify_err(workspace.downgrade(), window, cx);
            });
        });
    })
    .detach();
}

impl ClaudeCode {
    /// Runs `claude` in a new terminal.
    pub(crate) fn new_terminal(
        &mut self,
        workspace: Entity<Workspace>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Task<Result<()>> {
        let project = workspace.read(cx).project().clone();
        if !project.read(cx).supports_terminal(cx) {
            return Task::ready(Err(anyhow!(
                "terminal not yet supported for remote projects"
            )));
        }
        let working_directory = default_working_directory(workspace.read(cx), cx);
        let terminal = project.update(cx, |project, cx| {
            project.create_terminal_shell(working_directory, cx)
        });

        // `update_in` needs an entity that some window renders, which this
        // one isn't.
        let window = window.window_handle();
        cx.spawn(async move |this, cx| {
            let terminal = terminal.await?;
            window.update(cx, |_, window, cx| {
                this.update(cx, |this, cx| {
                    this.add_terminal(terminal, &workspace, window, cx)
                })
            })?
        })
    }

    /// Shows the terminal and starts `claude` in it. The terminals all open in
    /// one pane, split off the active one the first time and again whenever
    /// the user has closed it.
    fn add_terminal(
        &mut self,
        terminal: Entity<Terminal>,
        workspace: &Entity<Workspace>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let pane = dedicated_pane(&mut self.terminal_panes, workspace, window, cx);
        workspace.update(cx, |workspace, cx| {
            let terminal_view = cx.new(|cx| {
                TerminalView::new(
                    terminal.clone(),
                    workspace.weak_handle(),
                    workspace.database_id(),
                    workspace.project().downgrade(),
                    window,
                    cx,
                )
            });
            // A shell can take a moment to start; leave a modal the user has
            // opened in the meantime alone.
            let focus = !workspace.has_active_modal(window, cx);
            workspace.add_item(
                pane,
                Box::new(terminal_view),
                None,
                focus,
                focus,
                window,
                cx,
            );
        });
        run_cli(&terminal, cx);
    }
}

/// Types the command into the shell once it is ready for input, the way the
/// Agent Panel runs `agent.terminal_init_command`.
fn run_cli(terminal: &Entity<Terminal>, cx: &mut App) {
    let startup = terminal.update(cx, |terminal, _| {
        terminal.start_init_command_startup_handshake()
    });
    let terminal = terminal.downgrade();
    cx.spawn(async move |cx| {
        // The command is still typed if the shell never echoes the marker.
        let timeout = cx.background_executor().timer(SHELL_STARTUP_TIMEOUT);
        futures::select_biased! {
            _ = startup.fuse() => {}
            _ = timeout.fuse() => {}
        }

        // CR, not "\r\n", which puts PowerShell into continuation mode.
        let input = format!("{CLI_COMMAND}\r").into_bytes();
        if let Err(error) = terminal.update(cx, |terminal, cx| {
            if !terminal.write_init_command_after_startup(input, cx) {
                log::debug!("not running {CLI_COMMAND}: the terminal is already in use");
            }
        }) {
            log::debug!("not running {CLI_COMMAND}: the terminal closed: {error}");
        }
    })
    .detach();
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ClaudeCodeSettings;
    use gpui::{Focusable as _, TestAppContext, UpdateGlobal as _, VisualTestContext};
    use project::{FakeFs, Project};
    use settings::SettingsStore;
    use terminal::{
        TerminalBuilder,
        terminal_settings::{AlternateScroll, CursorShape},
    };
    use util::paths::PathStyle;
    use workspace::{MultiWorkspace, Pane};

    async fn init_test(
        cx: &mut TestAppContext,
    ) -> (
        Entity<ClaudeCode>,
        Entity<Workspace>,
        &mut VisualTestContext,
    ) {
        cx.update(|cx| {
            let settings_store = SettingsStore::test(cx);
            cx.set_global(settings_store);
            theme_settings::init(theme::LoadThemes::JustBase, cx);
            // Keep the tests off the network.
            SettingsStore::update_global(cx, |store, _| {
                store.override_global(ClaudeCodeSettings { enabled: false })
            });
        });

        // No folders: a shell can't start in one that only `FakeFs` has.
        let project = Project::test(FakeFs::new(cx.executor()), [], cx).await;
        let claude_code = cx.new(ClaudeCode::new);
        let (multi_workspace, cx) =
            cx.add_window_view(|window, cx| MultiWorkspace::test_new(project, window, cx));
        let workspace =
            multi_workspace.read_with(cx, |multi_workspace, _| multi_workspace.workspace().clone());
        (claude_code, workspace, cx)
    }

    /// Adds a terminal that has no shell behind it.
    fn add_terminal(
        claude_code: &Entity<ClaudeCode>,
        workspace: &Entity<Workspace>,
        cx: &mut VisualTestContext,
    ) {
        let terminal = cx.new(|cx| {
            TerminalBuilder::new_display_only(
                CursorShape::default(),
                AlternateScroll::On,
                None,
                0,
                cx.background_executor(),
                PathStyle::local(),
            )
            .subscribe(cx)
        });
        cx.update(|window, cx| {
            claude_code.update(cx, |this, cx| {
                this.add_terminal(terminal, workspace, window, cx)
            })
        });
        cx.run_until_parked();
    }

    fn terminal_count(pane: &Entity<Pane>, cx: &mut VisualTestContext) -> usize {
        pane.read_with(cx, |pane, _| pane.items_of_type::<TerminalView>().count())
    }

    #[gpui::test]
    async fn test_action_is_available_in_workspaces(cx: &mut TestAppContext) {
        let (_claude_code, _workspace, cx) = init_test(cx).await;
        cx.update(|window, cx| {
            window.draw(cx).clear(cx);
            assert!(window.is_action_available(&NewTerminal, cx));
        });
    }

    #[gpui::test]
    async fn test_terminals_open_in_a_pane_of_their_own(cx: &mut TestAppContext) {
        let (claude_code, workspace, cx) = init_test(cx).await;
        let working_pane = workspace.read_with(cx, |workspace, _| {
            assert_eq!(workspace.panes().len(), 1);
            workspace.active_pane().clone()
        });

        add_terminal(&claude_code, &workspace, cx);
        let terminal_pane = workspace.read_with(cx, |workspace, _| {
            assert_eq!(
                workspace.panes().len(),
                2,
                "the terminal split off a new pane"
            );
            workspace.active_pane().clone()
        });
        assert_ne!(terminal_pane, working_pane);
        assert_eq!(terminal_count(&working_pane, cx), 0);
        assert_eq!(terminal_count(&terminal_pane, cx), 1);

        // Back to work in the other pane; the next terminal still goes to the
        // terminal pane.
        cx.update(|window, cx| working_pane.focus_handle(cx).focus(window, cx));
        cx.run_until_parked();
        workspace.read_with(cx, |workspace, _| {
            assert_eq!(workspace.active_pane(), &working_pane);
        });
        add_terminal(&claude_code, &workspace, cx);
        workspace.read_with(cx, |workspace, _| {
            assert_eq!(workspace.panes().len(), 2);
            assert_eq!(workspace.active_pane(), &terminal_pane);
        });
        assert_eq!(terminal_count(&terminal_pane, cx), 2);
    }

    #[cfg(unix)]
    #[gpui::test]
    async fn test_new_terminal_runs_the_cli(cx: &mut TestAppContext) {
        use settings::Settings as _;
        use terminal::terminal_settings::TerminalSettings;

        let (claude_code, workspace, cx) = init_test(cx).await;
        // The terminal is a real one.
        cx.executor().allow_parking();
        cx.update(|_, cx| {
            // `cat` stands in for the shell, so that what gets typed is only
            // echoed and the tests don't start the real CLI.
            let mut terminal_settings = TerminalSettings::get_global(cx).clone();
            terminal_settings.shell = task::Shell::Program("/bin/cat".to_string());
            TerminalSettings::override_global(terminal_settings, cx);
        });

        cx.update(|window, cx| {
            claude_code.update(cx, |this, cx| {
                this.new_terminal(workspace.clone(), window, cx)
            })
        })
        .await
        .unwrap();
        cx.run_until_parked();
        let terminal = workspace
            .read_with(cx, |workspace, cx| {
                assert_eq!(workspace.panes().len(), 2);
                workspace.active_item_as::<TerminalView>(cx)
            })
            .expect("the terminal should be open")
            .read_with(cx, |terminal_view, _| terminal_view.terminal().clone());

        // `cat` never reports in the way a shell does.
        cx.executor().advance_clock(SHELL_STARTUP_TIMEOUT);
        cx.run_until_parked();
        let typed = terminal.update(cx, |terminal, _| terminal.take_input_log());
        assert_eq!(typed, vec![b"claude\r".to_vec()]);
    }
}
