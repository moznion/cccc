
            object M {
                def f(xs: List[Int]): List[Int] = xs.collect {
                    case x if x > 0 && x < 9 => x
                    case _ => 0
                }
            }
        