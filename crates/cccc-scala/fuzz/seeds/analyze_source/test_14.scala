
object S:
  def f(a: Boolean, n: Int): Int =
    if a then 1
    else if n > 0 then 2
    else n match
      case 0 => 3
      case _ => 4
