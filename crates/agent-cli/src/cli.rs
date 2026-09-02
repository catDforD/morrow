use super::*;

#[derive(Debug, Parser)]
#[command(name = "morrow")]
#[command(about = "Minimal OpenAI-compatible agent loop CLI")]
pub(crate) struct Args {
    #[arg(long)]
    pub(crate) config: Option<PathBuf>,

    #[arg(long)]
    pub(crate) session: Option<String>,

    #[arg(long, help = "Deprecated alias for --session")]
    pub(crate) thread: Option<String>,

    #[arg(long)]
    pub(crate) reset_session: bool,

    #[arg(long, help = "Deprecated alias for --reset-session")]
    pub(crate) reset_thread: bool,

    #[arg(long, value_parser = parse_permission_mode)]
    pub(crate) permission: Option<PermissionMode>,

    #[arg(long)]
    pub(crate) allow_shell: bool,

    #[arg(long)]
    pub(crate) jsonl: bool,

    #[command(subcommand)]
    pub(crate) command: Option<CliCommand>,

    #[arg(value_name = "PROMPT", num_args = 0..)]
    pub(crate) prompt: Vec<String>,
}

#[derive(Debug, Subcommand)]
pub(crate) enum CliCommand {
    Init {
        #[arg(long)]
        force: bool,
        #[arg(long)]
        template: bool,
    },
    Server {
        #[arg(long, default_value = "127.0.0.1")]
        host: IpAddr,
        #[arg(long, default_value_t = 3000)]
        port: u16,
        #[arg(
            long,
            help = "Disable browser session authentication (local debugging only)"
        )]
        no_auth: bool,
        #[arg(
            long,
            value_parser = parse_permission_mode,
            help = "Cap the permission mode web clients may request per turn"
        )]
        permission_ceiling: Option<PermissionMode>,
    },
    Session {
        #[command(subcommand)]
        command: SessionCommand,
    },
    Hooks {
        #[command(subcommand)]
        command: HooksCommand,
    },
}

#[derive(Debug, Subcommand)]
pub(crate) enum HooksCommand {
    List {
        #[arg(long)]
        json: bool,
    },
    Trust,
    Revoke,
}

#[derive(Debug, Subcommand)]
pub(crate) enum SessionCommand {
    List,
    Show {
        name: Option<String>,
    },
    Delete {
        name: String,
    },
    Rename {
        old: String,
        new: String,
    },
    Export {
        name: Option<String>,
        #[arg(long)]
        output: Option<PathBuf>,
    },
}
