function num(s) { return s ~ /^[0-9]+$/ }
NF == 3 && num($1) && num($2) && num($3) && $1 + 0 > 1 {
  p = $1 + 0
  pp[p] = $2 + 0
  pg[p] = $3 + 0
  seen[p] = 1
}
END {
  if (grp + 0 <= 1 || self + 0 <= 1) exit
  inS[self + 0] = 1
  do {
    changed = 0
    for (p in seen) if (!(p in inS) && (pp[p] in inS)) { inS[p] = 1; changed = 1 }
  } while (changed)
  par = parent + 0
  mpg = (par in seen) ? pg[par] : 0
  for (p in seen) {
    if (p in inS || p == par) continue
    if (pg[p] == grp + 0 || (leader != "" && p == leader + 0)) inD[p] = 1
  }
  do {
    changed = 0
    for (p in seen) if (!(p in inD) && !(p in inS) && p != par && (pp[p] in inD)) { inD[p] = 1; changed = 1 }
  } while (changed)
  for (p in inD) {
    g = pg[p]
    if (g == grp + 0 || g == self + 0) continue
    if (g > 1 && g != mpg && (g in inD)) {
      if (!(g in done)) { done[g] = 1; print "group", g }
    } else print "pid", p
  }
}
