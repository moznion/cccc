
            class C {
                void f() {
                    try {
                        risky();
                    } catch (IllegalStateException e) {
                    } catch (RuntimeException | Error e) {
                    } finally {
                        cleanup();
                    }
                }
            }
        