use crate::cli::Shell;

/// A wrapper function for `eval "$(wtm shell zsh)"`. It changes directory
/// after `new` and `cd` and execs the binary unchanged for everything else.
///
/// Exit code 3 means the worktree was created but its init hook failed, so
/// the wrapper still changes directory. The directory exists and is usable.
/// It prints the path instead only when there is no directory to enter.
pub fn wrapper(shell: Shell) -> &'static str {
    match shell {
        Shell::Zsh | Shell::Bash => POSIX,
        Shell::Fish => FISH,
    }
}

const POSIX: &str = r#"wtm() {
  case "$1" in
    new|cd)
      local out rc
      out="$(command wtm "$@")"
      rc=$?
      if [ -d "$out" ]; then
        cd "$out" || return $?
      elif [ -n "$out" ]; then
        printf '%s\n' "$out"
      fi
      return $rc
      ;;
    *)
      command wtm "$@"
      ;;
  esac
}"#;

const FISH: &str = r#"function wtm
    switch "$argv[1]"
        case new cd
            set -l out (command wtm $argv)
            set -l rc $status
            if test -d "$out"
                cd "$out"
            else if test -n "$out"
                printf '%s\n' "$out"
            end
            return $rc
        case '*'
            command wtm $argv
    end
end"#;
