
            class Foo {
                void bar(int retry);
            };
            void Foo::bar(int retry) {
                if (retry) {
                    Foo::bar(0);
                }
            }
        