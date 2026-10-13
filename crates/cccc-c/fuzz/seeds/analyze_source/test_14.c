
            void f(int a) {
            #ifdef FAST_PATH
                if (a) { }
            #elif defined(SLOW_PATH)
                while (a) { }
            #else
                (void)a;
            #endif
            }
        