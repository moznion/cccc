<?php
            function sumOfPrimes($max) {
                $total = 0;
                for ($i = 2; $i <= $max; $i++) {
                    for ($j = 2; $j < $i; $j++) {
                        if ($i % $j == 0) {
                            continue 2;
                        }
                    }
                    $total += $i;
                }
                return $total;
            }
        