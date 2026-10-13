<?php
            class S {
                static function fib($n) {
                    if ($n < 2) {
                        return $n;
                    }
                    return self::fib($n - 1) + self::fib($n - 2);
                }
            }
        