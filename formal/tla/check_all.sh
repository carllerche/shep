#!/bin/sh
# Runs every model/config and compares the outcome with the expected one.
# Usage: TLA2TOOLS=/path/to/tla2tools.jar ./check_all.sh [quick]
#   quick: skip the configs that take more than ~2 minutes.
cd "$(dirname "$0")"
QUICK="$1"
fail=0
run() { # spec cfg expected(pass|fail) slow?
  if [ "$QUICK" = quick ] && [ "$4" = slow ]; then echo "skip  $1 $2"; return; fi
  start=$(date +%s)
  out=$(./run.sh "$1" "$2" 2>&1)
  secs=$(( $(date +%s) - start ))
  if echo "$out" | grep -q "No error has been found"; then got=pass
  elif echo "$out" | grep -qE "is violated|was violated"; then got=fail
  else got=error; fi
  states=$(echo "$out" | grep -E "distinct states found" | tail -1 | sed 's/.* \([0-9]*\) distinct states found.*/\1/')
  what=$(echo "$out" | grep -E "is violated|was violated" | head -1 | sed 's/^Error: //')
  if [ "$got" = "$3" ]; then mark=ok; else mark=UNEXPECTED; fail=1; fi
  printf '%-10s %-15s %-32s expected=%s got=%s distinct=%s %ss %s\n' "$mark" "$1" "$2" "$3" "$got" "$states" "$secs" "$what"
}
run DotFrontier    DotFrontier.cfg             pass slow
run DotFrontier    DotFrontier_deep.cfg        pass slow
run DotFrontier    DotFrontier_live.cfg        pass slow
run DotFrontier    DotFrontier_ooo.cfg         fail
run DotFrontier    DotFrontier_hang.cfg        fail
run DotFrontier    DotFrontier_noseq.cfg       fail
run DotFrontier    DotFrontier_compact.cfg     fail
run SerialLog      SerialLog.cfg               pass
run SerialLog      SerialLog_fix.cfg           fail
run SerialLog      SerialLog_fix_ooo.cfg       fail
run MultiNode      MultiNode.cfg               pass slow
run MultiNode      MultiNode_read.cfg          fail
run MultiNode      MultiNode_closure.cfg       fail
run ViewMaintainer ViewMaintainer.cfg          pass slow
run ViewMaintainer ViewMaintainer_live.cfg     pass
run ViewMaintainer ViewMaintainer_cursor.cfg   fail
run ViewMaintainer ViewMaintainer_untailed.cfg fail
run ViewMaintainer ViewMaintainer_changes.cfg  fail
run ChangeQueries  ChangeQueries.cfg           pass
run ChangeQueries  ChangeQueries_handle.cfg    fail
run ChangeQueries  ChangeQueries_window.cfg    fail
run SlackRT        SlackRT.cfg                 pass
run SlackRT        SlackRT_single.cfg          pass
run SlackRT        SlackRT_poll.cfg            fail
run SlackRT        SlackRT_poll_push.cfg       fail
run SlackRT        SlackRT_failover.cfg        fail
run SlackRT        SlackRT_connect.cfg         fail
run SlackRT        SlackRT_token.cfg           fail
exit $fail
