
            class C {
                @get:[JvmStatic JvmName("treeCount")]
                val treeCount: Int get() = 1

                fun f(x: Boolean): Int {
                    if (x) return 1
                    return 0
                }
            }
        