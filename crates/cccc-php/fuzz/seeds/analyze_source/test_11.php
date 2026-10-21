<?php
            function f() {
                try {
                    risky();
                } catch (\Exception $e) {
                    handle();
                }
            }
        