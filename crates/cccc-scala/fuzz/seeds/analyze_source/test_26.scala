
            object S {
                def f(o: Option[Int]): Int = o match {
                    case Some(x) => x
                    case None => 0
                }
            }
        