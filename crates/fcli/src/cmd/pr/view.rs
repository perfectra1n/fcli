//! `fcli pr view` — one pull request, in words.

use std::io::Write;

use clap::Args as ClapArgs;
use forgejo_core::Result;

use super::common;
use crate::cmd::support;
use crate::global::GlobalOpts;
use crate::output::color::{paint, style_by_name};
use crate::runtime::Runtime;

#[derive(Debug, ClapArgs)]
#[command(long_about = "\
Show a pull request.

Without an argument, uses the pull request for the current branch.

  fcli pr view
  fcli pr view 42 --comments
  fcli pr view my-branch
  fcli pr view -w
  fcli pr view --json state,mergeable,head")]
pub struct Args {
    /// Pull request number, URL, or branch. Defaults to the branch you are on
    #[arg(value_name = "PR")]
    pub pr: Option<String>,

    /// Open in a browser instead of printing
    #[arg(short = 'w', long)]
    pub web: bool,

    /// Print the comments too
    #[arg(short = 'c', long)]
    pub comments: bool,
}

pub fn run(globals: &GlobalOpts, args: &Args) -> Result<()> {
    let wanted = support::machine::plan(globals, forgejo_client::fields::FIELDS_PULL_REQUEST)?;
    if matches!(wanted, support::machine::Wanted::Listed) {
        return Ok(());
    }
    crate::runtime::block_on(async move {
        let rt = Runtime::new(globals)?;
        let api = support::api(&rt);
        let found = common::find(&rt, globals, &api, args.pr.as_deref()).await?;

        if args.web {
            return support::open_web(&rt, &found.pr.html_url);
        }

        let comments = if args.comments {
            let query = forgejo_client::query::IssueGetCommentsQuery::default();
            api.issue()
                .get_comments(&found.slug.owner, &found.slug.name, found.index(), &query)
                .await?
        } else {
            Vec::new()
        };

        common::emit_or(&rt, globals, &wanted, &found.pr, || {
            let mut out = std::io::stdout().lock();
            out.write_all(common::detail(&found.pr, rt.term(), true).as_bytes())?;
            if args.comments {
                out.write_all(render_comments(&comments, rt.term()).as_bytes())?;
            }
            out.flush()?;
            Ok(())
        })
    })
}

/// Comments, oldest first, each with its author and age.
///
/// Not routed through `Table`: a comment body is prose of arbitrary length, and a table would
/// truncate it to fit a column — which is the one thing that must not happen to the text somebody is
/// reading the command to see.
fn render_comments(comments: &[forgejo_model::Comment], term: &crate::output::Term) -> String {
    let dim = style_by_name("gray").unwrap_or_default();
    let bold = style_by_name("bold").unwrap_or_default();
    if comments.is_empty() {
        return paint(term, dim, "\nNo comments.\n");
    }
    let mut out = String::new();
    for comment in comments {
        let who = comment.user.as_ref().map(|u| u.login.clone()).unwrap_or_default();
        let when = comment
            .created_at
            .map(|ts| crate::output::template::funcs::timeago(&ts.to_string()))
            .unwrap_or_default();
        out.push('\n');
        out.push_str(&paint(term, bold, &who));
        out.push_str(&paint(term, dim, &format!(" • {when}\n")));
        for line in comment.body.lines() {
            out.push_str(&format!("  {line}\n"));
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::output::Term;
    use forgejo_core::types::ids::{CommentId, IssueIndex};
    use forgejo_model::{Comment, PrBranchInfo, PullRequest, StateType, User};

    fn pr() -> PullRequest {
        PullRequest {
            number: IssueIndex::new(42),
            title: "Teach the parser about tabs".to_owned(),
            body: "Fixes #12\n\nThe lexer treated a tab as one column.".to_owned(),
            state: StateType::Open,
            additions: 24,
            deletions: 3,
            user: Some(User { login: "alice".to_owned(), ..User::default() }),
            base: Some(PrBranchInfo { r#ref: "main".to_owned(), ..PrBranchInfo::default() }),
            head: Some(PrBranchInfo {
                r#ref: "tabs".to_owned(),
                label: "alice:tabs".to_owned(),
                ..PrBranchInfo::default()
            }),
            html_url: "https://git.example.org/them/proj/pulls/42".to_owned(),
            ..PullRequest::default()
        }
    }

    #[test]
    fn detail_view_snapshot() {
        insta::assert_snapshot!(common::detail(&pr(), &Term::tty(80), true));
    }

    /// An AGit pull request has no head branch in the repository, which changes what `pr checkout`
    /// and `pr merge -d` can do — so the view has to say so.
    #[test]
    fn an_agit_pull_request_is_labelled_as_one() {
        let agit = PullRequest { flow: 1, ..pr() };
        let out = common::detail(&agit, &Term::tty(80), false);
        assert!(out.contains("AGit"), "{out}");
    }

    /// Bug this prevents: an empty description rendering as a blank gap, so the reader cannot tell
    /// it apart from output that got cut off.
    #[test]
    fn an_empty_body_says_so() {
        let empty = PullRequest { body: String::new(), ..pr() };
        let out = common::detail(&empty, &Term::tty(80), true);
        assert!(out.contains("No description provided."), "{out}");
    }

    #[test]
    fn comments_render_with_author_and_age() {
        let comments = vec![Comment {
            id: CommentId::new(1),
            body: "Looks right to me.".to_owned(),
            user: Some(User { login: "bob".to_owned(), ..User::default() }),
            ..Comment::default()
        }];
        let out = render_comments(&comments, &Term::tty(80));
        assert!(out.contains("bob"), "{out}");
        assert!(out.contains("  Looks right to me."), "{out}");
        assert!(render_comments(&[], &Term::tty(80)).contains("No comments"));
    }
}
