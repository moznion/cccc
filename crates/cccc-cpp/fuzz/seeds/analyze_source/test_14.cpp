
            template <>
            void spec<int>(int x) {
                if (x) { spec<int>(0); }
            }
            template <>
            int Foo<int>::get<char>() {
                if (a) { }
                return 0;
            }
        