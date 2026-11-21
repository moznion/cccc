outer() {
  if a; then
    inner() { if b; then c; fi; }
  fi
}
