
            void f(int a) {
                for (;;) {
                    if (a) {
                        break;
                    }
                    goto out;
                }
            out:;
            }
        