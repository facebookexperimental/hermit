# shellcheck shell=bash
#
# Let a program that a demo runs under `hermit run` reach its own executable
# and the files named on its command line when they are under the host's /tmp.
#
# `hermit run` gives the program a private, empty /tmp, so the program cannot
# see anything under the host's /tmp, and Hermit refuses to start a program
# that is there. `hermit run --tmp=/tmp` would show the program everything in
# the host's /tmp, which would make all of it an input to the run. Instead,
# these functions pass `--bind` for exactly what the program needs: the
# directory that holds the program, and each other file by itself. Hermit
# mounts each one at the same path inside the private /tmp. Nothing is added
# for a path outside /tmp, so the demo's command line for a checkout outside
# /tmp is the same as without these functions.
#
# The program is bound through its directory because Hermit cannot start a
# program whose path is itself a --bind target. Hermit looks the program up on
# the host as the bind's source joined with the rest of the program's path
# after the bind's target (mapped_path in hermit-cli/src/bin/hermit/run.rs).
# When the program is the target, the rest is empty, the join leaves the path
# with a trailing slash, and a file path with a trailing slash is not found
# (ENOTDIR), so Hermit reports that the program does not exist
# (https://github.com/rrnewton/hermit/issues/3626). Binding the directory also
# shows the program the directory's other entries, which in the
# demos are the program alone or files tracked in the repository, so a FILE
# inside that directory is not bound again.
#
# The program resolves a symbolic link inside the bound directory in its own
# view, where the rest of the host's /tmp is hidden, so a link to another place
# under /tmp leads nowhere there. The program, and each FILE inside its
# directory, is therefore refused when the program itself or a component of the
# FILE's path below that directory is a symbolic link. This also refuses links
# that would work, such as one to a path outside /tmp or to another file in the
# same directory; pass the path the link resolves to instead. A FILE outside
# the program's directory is bound by itself, and a symbolic link in its path
# is no obstacle: Hermit mounts it before it hides the host's /tmp, and the
# mount follows the link, so the program sees the file the link points to.
#
# hermit_tmp_check_paths HINT PROGRAM [FILE...]
#   Return 0 if PROGRAM and every FILE are outside /tmp or can be shown to the
#   program. Otherwise print an error about the first that cannot, followed by
#   HINT, on standard error and return 1.
#
#   A path that begins with /tmp/ can be shown by binding it unless it contains
#   `:` (which --bind reads as SOURCE:TARGET) or a `..` component (Hermit puts
#   the bound path at the same relative path inside the private /tmp, and `..`
#   would leave it), or it names /tmp itself. For PROGRAM, its directory is
#   bound, so PROGRAM must not be directly in /tmp (binding /tmp would show the
#   program all of it), its directory must not be a symbolic link (Hermit
#   would mount it as a file), and PROGRAM must not be a symbolic link itself.
#   A FILE inside PROGRAM's directory must have no symbolic link among the
#   components of its path below that directory (see above).
#
#   A path that reaches /tmp without beginning with /tmp/ is refused when it is
#   absolute (through a symbolic link), or when it is PROGRAM (Hermit starts a
#   program by the absolute path it resolves on the host, which then begins
#   with /tmp/ and is hidden). A relative FILE is looked up by the program from
#   its working directory, which it inherits as the directory itself, not its
#   name. That lookup sees the host's files, one component at a time, until it
#   steps onto /tmp itself, by a `..` from below /tmp or by `tmp` from /, where
#   the program sees its private /tmp, or until it meets a symbolic link, which
#   the program resolves in its own view. A relative FILE that reaches /tmp is
#   therefore refused when one of its components is a symbolic link or its
#   lookup passes through /tmp itself, and is otherwise left unchanged,
#   including a `..` that stays below /tmp, as in `sub/../a.img` or
#   `../dir/a.img`.
#
# hermit_tmp_bind_args ARRAY PROGRAM [FILE...]
#   Set the caller's array named ARRAY to `--bind DIR` for PROGRAM's directory
#   when it can be bound, followed by `--bind FILE` for each FILE that can be
#   bound and is not inside that directory, and to nothing else. It never
#   fails; call hermit_tmp_check_paths on the same paths first, so that a path
#   that cannot be shown stops the demo instead of being left out.
#
# hermit_tmp_is_under PATH
#   Return 0 if PATH begins with /tmp/ or resolves to /tmp or a path under it.

# Classify the path $2, which is the program when $1 is "program" and another
# file when $1 is "file". Set _HERMIT_TMP_CLASS to "bind", "unchanged", or
# "refuse"; for "bind", _HERMIT_TMP_BIND to the path to pass to --bind and
# _HERMIT_TMP_NORMAL to that path without `.` components or repeated or
# trailing slashes; for "refuse", _HERMIT_TMP_PROBLEM to the reason.
_hermit_tmp_classify() {
  local kind="$1" path="$2" resolved prefix rest component
  _HERMIT_TMP_CLASS=unchanged
  _HERMIT_TMP_BIND=
  _HERMIT_TMP_NORMAL=
  _HERMIT_TMP_PROBLEM=
  case "$path" in
    /tmp/*)
      _HERMIT_TMP_CLASS=refuse
      case "$path" in
        *:*)
          _HERMIT_TMP_PROBLEM="it contains ':', which --bind reads as SOURCE:TARGET"
          return
          ;;
      esac
      case "/$path/" in
        */../*)
          _HERMIT_TMP_PROBLEM="it contains a '..' component"
          return
          ;;
      esac
      if [ "$kind" = program ]; then
        _HERMIT_TMP_BIND=$(dirname -- "$path")
      else
        _HERMIT_TMP_BIND=$path
      fi
      # Remove `.` components and repeated or trailing slashes, without
      # following symbolic links, to catch a spelling of /tmp itself.
      if ! _HERMIT_TMP_NORMAL=$(realpath -m -s -- "$_HERMIT_TMP_BIND" 2>/dev/null); then
        _HERMIT_TMP_PROBLEM="realpath could not normalize $_HERMIT_TMP_BIND"
        return
      fi
      if [ "$_HERMIT_TMP_NORMAL" = /tmp ]; then
        if [ "$kind" = program ]; then
          _HERMIT_TMP_PROBLEM="it is directly in /tmp, and binding its directory would show the program all of /tmp"
        else
          _HERMIT_TMP_PROBLEM="it names /tmp itself, not a file below it"
        fi
        return
      fi
      if [ "$kind" = program ] && [ -L "$_HERMIT_TMP_BIND" ]; then
        _HERMIT_TMP_PROBLEM="its directory, $_HERMIT_TMP_BIND, is a symbolic link, which Hermit would mount as a file"
        return
      fi
      if [ "$kind" = program ] && [ -L "$_HERMIT_TMP_NORMAL/$(basename -- "$path")" ]; then
        _HERMIT_TMP_PROBLEM="it is a symbolic link, which Hermit would follow in the program's view, where the rest of the host's /tmp is hidden; $(_hermit_tmp_resolves_to "$path")"
        return
      fi
      _HERMIT_TMP_CLASS=bind
      ;;
    *)
      # realpath -m resolves the symbolic links and `..` in the part of the
      # path that exists and does not require the rest to exist. If it fails
      # (a symbolic-link loop, an unreadable directory), the path is passed
      # on unchanged, as it was before these functions existed.
      resolved=$(realpath -m -- "$path" 2>/dev/null) || return 0
      case "$resolved/" in
        /tmp/*) ;;
        *) return 0 ;;
      esac
      _HERMIT_TMP_CLASS=refuse
      case "$path" in
        /*)
          _HERMIT_TMP_PROBLEM="it resolves to $resolved, under /tmp, but it does not begin with /tmp/"
          return
          ;;
      esac
      if [ "$kind" = program ]; then
        _HERMIT_TMP_PROBLEM="Hermit starts a program by the absolute path it resolves on the host, $resolved, which is under /tmp"
        return
      fi
      # Follow the program's lookup of the relative path, one component at a
      # time from the physical path of the working directory, which the
      # program inherits as the directory itself. A `..` moves to the parent
      # directory, as it does for the program; stepping onto /tmp itself, by a
      # `..` from below it or by `tmp` from /, moves the program into its
      # private /tmp, and a symbolic link is resolved in the program's view.
      if ! prefix=$(pwd -P 2>/dev/null); then
        _HERMIT_TMP_PROBLEM="the working directory could not be resolved"
        return
      fi
      rest=$path
      while [ -n "$rest" ]; do
        component=${rest%%/*}
        if [ "$component" = "$rest" ]; then
          rest=
        else
          rest=${rest#*/}
        fi
        case "$component" in
          '' | .)
            continue
            ;;
          ..)
            prefix=${prefix%/*}
            if [ -z "$prefix" ]; then
              prefix=/
            fi
            ;;
          *)
            prefix=${prefix%/}/$component
            if [ -L "$prefix" ]; then
              _HERMIT_TMP_PROBLEM="it resolves to $resolved, under /tmp, through a symbolic link"
              return
            fi
            ;;
        esac
        if [ "$prefix" = /tmp ]; then
          _HERMIT_TMP_PROBLEM="it resolves to $resolved, under /tmp, through /tmp itself, where the program sees its private /tmp"
          return
        fi
      done
      _HERMIT_TMP_CLASS=unchanged
      ;;
  esac
}

# Print the end of a refusal of the symbolic link $1: the instruction to give
# the path it resolves to, naming that path when realpath can resolve it.
_hermit_tmp_resolves_to() {
  local resolved
  if resolved=$(realpath -m -- "$1" 2>/dev/null); then
    printf 'give the path it resolves to, %s, instead' "$resolved"
  else
    printf 'give the path it resolves to instead'
  fi
}

# Set _HERMIT_TMP_LINK to the first symbolic link among the paths that lead
# from the directory $1 down to $2, a path inside it without `.` or `..`
# components or repeated or trailing slashes, or to nothing if there is none.
_hermit_tmp_link_below() {
  local prefix="$1" rest="${2#"$1"}" component
  _HERMIT_TMP_LINK=
  while [ -n "$rest" ]; do
    rest=${rest#/}
    component=${rest%%/*}
    rest=${rest#"$component"}
    prefix=$prefix/$component
    if [ -L "$prefix" ]; then
      _HERMIT_TMP_LINK=$prefix
      return 0
    fi
  done
  return 0
}

# Classify the file $2 as _hermit_tmp_classify does, when $1 is the program's
# directory, bound as a whole, or empty when that directory is not bound. A
# file that the directory already shows gets the class "inside" and is not
# bound again, unless the program would meet a symbolic link below the
# directory on the way to it, which refuses it.
_hermit_tmp_classify_file() {
  local program_dir="$1"
  _hermit_tmp_classify file "$2"
  if [ "$_HERMIT_TMP_CLASS" != bind ] || [ -z "$program_dir" ]; then
    return 0
  fi
  case "$_HERMIT_TMP_NORMAL/" in
    "$program_dir"/*) ;;
    *) return 0 ;;
  esac
  _hermit_tmp_link_below "$program_dir" "$_HERMIT_TMP_NORMAL"
  if [ -z "$_HERMIT_TMP_LINK" ]; then
    _HERMIT_TMP_CLASS=inside
    return 0
  fi
  _HERMIT_TMP_CLASS=refuse
  _HERMIT_TMP_PROBLEM="it is inside the program's directory, $program_dir, which is bound as a whole, and $_HERMIT_TMP_LINK is a symbolic link, which the program would follow in its own view, where the rest of the host's /tmp is hidden; $(_hermit_tmp_resolves_to "$2")"
  return 0
}

hermit_tmp_check_paths() {
  local hint="$1" kind=program path program_dir=
  shift
  for path in "$@"; do
    if [ "$kind" = program ]; then
      _hermit_tmp_classify program "$path"
      if [ "$_HERMIT_TMP_CLASS" = bind ]; then
        program_dir=$_HERMIT_TMP_NORMAL
      fi
      kind="file"
    else
      _hermit_tmp_classify_file "$program_dir" "$path"
    fi
    if [ "$_HERMIT_TMP_CLASS" = refuse ]; then
      printf 'error: %s is under /tmp. Hermit gives the program it runs a private /tmp, and the demo cannot make this path visible there: %s. %s\n' \
        "$path" "$_HERMIT_TMP_PROBLEM" "$hint" >&2
      return 1
    fi
  done
  return 0
}

hermit_tmp_bind_args() {
  local -n _hermit_tmp_binds="$1"
  local path program_dir=
  shift
  _hermit_tmp_binds=()
  [ "$#" -gt 0 ] || return 0
  _hermit_tmp_classify program "$1"
  shift
  if [ "$_HERMIT_TMP_CLASS" = bind ]; then
    _hermit_tmp_binds+=(--bind "$_HERMIT_TMP_BIND")
    program_dir=$_HERMIT_TMP_NORMAL
  fi
  for path in "$@"; do
    # The program's directory already shows a file "inside" it.
    _hermit_tmp_classify_file "$program_dir" "$path"
    [ "$_HERMIT_TMP_CLASS" = bind ] || continue
    _hermit_tmp_binds+=(--bind "$_HERMIT_TMP_BIND")
  done
  return 0
}

hermit_tmp_is_under() {
  local resolved
  case "$1" in
    /tmp/*) return 0 ;;
  esac
  resolved=$(realpath -m -- "$1" 2>/dev/null) || return 1
  case "$resolved/" in
    /tmp/*) return 0 ;;
  esac
  return 1
}
