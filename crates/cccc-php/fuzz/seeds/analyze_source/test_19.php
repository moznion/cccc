<?php
            function host($cb) {
                $obj = new class($cb, fn ($z) => $z && $z) {
                    public function m($x) {
                        if ($x) {
                            return 1;
                        }
                        return 0;
                    }
                    public function n($y) {
                        return $y ?? 0;
                    }
                };
                return $obj;
            }
        