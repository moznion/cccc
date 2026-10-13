<?php
            class S {
                function walk($n) {
                    if ($n == 0) {
                        return 0;
                    } else {
                        return $this->walk($n - 1);
                    }
                }
            }
        