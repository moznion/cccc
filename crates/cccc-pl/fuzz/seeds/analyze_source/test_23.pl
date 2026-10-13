
use feature 'class';
class Counter {
    field $count = 0;
    method increment {
        if ($count > 10) { return $count; }
        $count++;
        return $count;
    }
}
