
            fn free() {}
            struct C;
            impl C { fn method(&self) {} }
            fn host() { let lambda = |x: u32| x + 1; }
        