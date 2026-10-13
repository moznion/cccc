
            object S {
                def f(a: Boolean, items: List[Int]): Unit = {
                    while (a) { work() };
                    for (x <- items) { work() };
                    do { work() } while (a);
                }
            }
        