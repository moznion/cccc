<?php
            namespace Foo;
            function f($x) {
                if ($x) {
                    return 1;
                }
                return 0;
            }
            class C {
                public function m($y) {
                    foreach ($y as $i) {
                        if ($i) {
                            return $i;
                        }
                    }
                    return null;
                }
            }
        