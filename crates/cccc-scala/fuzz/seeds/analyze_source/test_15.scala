
object S:
  def f(): Int =
    try risky()
    catch
      case e: IllegalStateException => 1
      case e: RuntimeException => 2
