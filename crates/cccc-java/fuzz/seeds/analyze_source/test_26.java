
            class C {
                @Override
                @SuppressWarnings({"unchecked", "deprecation"})
                int f(boolean a, boolean b) {
                    if (a && b) return 1;
                    return 0;
                }
            }
        