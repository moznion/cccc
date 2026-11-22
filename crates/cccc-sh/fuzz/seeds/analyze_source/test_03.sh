#!/usr/bin/env bats

setup() {
  load helper
}

@test "addition works" {
  run add 1 2
  if [ "$status" -ne 0 ]; then
    false
  fi
}

@test 'single quoted' { :; }

@test unquoted name { :; }
