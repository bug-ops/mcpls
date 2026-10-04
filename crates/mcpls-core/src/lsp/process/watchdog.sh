exec 5>&0 3>&1 4<&2 1>/dev/null 2>/dev/null
trap '' HUP INT TERM QUIT USR1 USR2 ALRM PIPE
G=$1 LEADER=$2 PS=$3 PROG=$4 POLLS=$5
LF='
'
TARGETS=$LF
SEQ=0
d=$(mktemp -d) || d=
if sleep 0.001 2>/dev/null; then
  NAP=0.02
else
  NAP=1
  POLLS=$(((POLLS + 49) / 50))
fi
scan() {
  SEQ=$((SEQ + 1))
  (
    "$PS" -A -o pid= -o ppid= -o pgid= >"$d/ps.$SEQ" &
    echo $! >"$d/pid.$SEQ"
    wait $! && : >"$d/ok.$SEQ"
    : >"$d/done.$SEQ"
  ) <&- >/dev/null 2>&1 3>&- 4>&- 5>&- &
  job=$!
  i=0
  while [ ! -e "$d/done.$SEQ" ] && [ "$i" -lt "$POLLS" ]; do
    sleep "$NAP"
    i=$((i + 1))
  done
  if [ ! -e "$d/done.$SEQ" ]; then
    read -r psid <"$d/pid.$SEQ" 2>/dev/null && kill -s KILL "$psid" 2>/dev/null
    kill -s KILL "$job" 2>/dev/null
    return 1
  fi
  [ -e "$d/ok.$SEQ" ] || return 2
  awk -v self=$$ -v grp="$G" -v leader="$LEADER" -v parent="$PPID" "$PROG" "$d/ps.$SEQ" \
    >"$d/out.$SEQ" 2>/dev/null 3>&- 4>&- 5>&-
}
anchor_alive() {
  kill -0 "$G" 2>/dev/null
}
freeze() {
  anchor_alive && kill -s STOP -- "-$G" 2>/dev/null
  [ -n "$d" ] || return 0
  pass=0
  while [ "$pass" -lt 3 ]; do
    pass=$((pass + 1))
    scan
    rc=$?
    if [ "$rc" -ne 0 ]; then
      if [ "$rc" -eq 1 ]; then
        printf 'scan-timeout\n' >&5
      else
        printf 'scan-failed\n' >&5
      fi
      return 0
    fi
    new=0
    while read -r kind id; do
      case $kind in group | pid) ;; *) continue ;; esac
      case $id in '' | *[!0-9]*) continue ;; esac
      [ "$id" -gt 1 ] || continue
      t="$kind $id"
      case $TARGETS in *"$LF$t$LF"*) continue ;; esac
      TARGETS="$TARGETS$t$LF"
      printf 'target %s\n' "$t" >&5
      case $kind in
        group) kill -s STOP -- "-$id" 2>/dev/null ;;
        *) kill -s STOP "$id" 2>/dev/null ;;
      esac
      new=1
    done <"$d/out.$SEQ"
    [ "$new" -eq 1 ] || return 0
  done
}
sweep() {
  freeze
  [ -z "$d" ] || rm -rf "$d"
  printf %s "$TARGETS" | while read -r kind id; do
    case $kind in
      group) kill -s KILL -- "-$id" 2>/dev/null ;;
      pid) kill -s KILL "$id" 2>/dev/null ;;
    esac
  done
  [ -z "$LEADER" ] || kill -s KILL "$LEADER" 2>/dev/null
  anchor_alive && kill -s KILL -- "-$G" 2>/dev/null
  exit 0
}
while read -r cmd; do
  case $cmd in
    mark)
      freeze
      anchor_alive && kill -s CONT -- "-$G" 2>/dev/null
      printf 'marked\n' >&5
      ;;
    forget-leader) LEADER= ;;
  esac
done
sweep
