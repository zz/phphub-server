<?php
// Unmodified front controller: built and torn down on every request.
$app = require dirname(__DIR__) . '/bootstrap.php';
$app->handle($_SERVER['REQUEST_URI'])->send();
