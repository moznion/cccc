<?php
            function host() {
                $pick = function ($a, $b) {
                    if ($a && $b) {
                        return 1;
                    } else {
                        return 0;
                    }
                };
                return $pick;
            }
        